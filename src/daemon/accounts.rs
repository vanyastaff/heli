//! The daemon's side of the build store (DESIGN.md §6.2): it learns each
//! workspace a run builds in, writes the owner manifest of its build
//! directory, and keeps the sizes the store's directories take.
//!
//! **Learning.** A run of cargo (any run but `exec`, and an `exec` of a
//! program named `cargo`) starts, beside it in the background,
//! `cargo metadata --format-version 1 --no-deps` as the run would: its
//! program, in its directory, with its environment. That names the
//! workspace root, its `target/` directory and its build directory, as the
//! run's configuration places them. When the build directory is in a store
//! heli manages (`crate::store::store_of`), heli writes its owner manifest
//! (`crate::store::owner`), with the run's start as the last run and the
//! repository git names for the workspace. Nothing of this delays the run,
//! and nothing of it can fail the run: what goes wrong goes to the daemon's
//! log. The workspace's `target/` directory is registered
//! (`crate::store::registry`) whether or not its build directory is in a
//! store.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;

use super::Log;
use crate::class::Class;
use crate::clock;
use crate::config::StoreConfig;
use crate::protocol::RunRequest;
use crate::store::registry::{Registry, Target};
use crate::store::{self, owner};

pub struct Accounts {
    config: StoreConfig,
    /// The state directory, which holds the registry of `target/`
    /// directories (`crate::store::registry`).
    state_dir: PathBuf,
    /// Held while the registry is rewritten: one writer at a time.
    registry: Mutex<()>,
    log: Log,
}

/// A workspace as cargo places it for a run.
#[derive(Debug, Deserialize)]
struct Workspace {
    #[serde(rename = "workspace_root")]
    root: PathBuf,
    #[serde(rename = "target_directory")]
    target: PathBuf,
    /// Absent from a cargo older than `build-dir`, whose build directory is
    /// the target directory.
    #[serde(rename = "build_directory")]
    build: Option<PathBuf>,
}

/// What it takes to ask about a run's workspace as the run would.
struct Probe {
    cwd: PathBuf,
    cargo: OsString,
    env: Vec<(OsString, OsString)>,
}

impl Accounts {
    pub(super) fn new(config: StoreConfig, state_dir: PathBuf, log: Log) -> Arc<Self> {
        Arc::new(Self {
            config,
            state_dir,
            registry: Mutex::new(()),
            log,
        })
    }

    /// Learns, in the background, the workspace that run `request` of
    /// `class` builds in (see the module docs).
    pub fn learn(self: &Arc<Self>, request: &RunRequest, class: Class) {
        let Some(probe) = Probe::of(request, class) else {
            return;
        };
        let at = clock::now();
        let accounts = Arc::clone(self);
        std::thread::spawn(move || accounts.discover(&probe, at));
    }

    /// Asks cargo where the workspace of `probe` builds, and records a run
    /// there that started `at`.
    fn discover(&self, probe: &Probe, at: u64) {
        let Some(workspace) = probe.workspace() else {
            return;
        };
        self.register(Target {
            path: workspace.target.clone(),
            workspace: workspace.root.clone(),
        });
        let build = workspace.build.as_ref().unwrap_or(&workspace.target);
        if store::store_of(&self.config, build).is_none() {
            return;
        }
        let repository = probe.repository(&workspace.root);
        if let Err(error) = owner::record_run(build, &workspace.root, repository.as_deref(), at) {
            self.log.write(&format!(
                "cannot write the owner manifest of {}: {error}",
                build.display()
            ));
        }
    }
}

impl Accounts {
    /// Registers `target`.
    fn register(&self, target: Target) {
        let _writing = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = Registry::add(&self.state_dir, target) {
            self.log
                .write(&format!("cannot register a target directory: {error}"));
        }
    }
}

impl Probe {
    /// The probe for run `request` of `class`, if the run is cargo's.
    fn of(request: &RunRequest, class: Class) -> Option<Self> {
        let program = &request.argv.first()?.0;
        let named_cargo = Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo"));
        if class == Class::Exec && !named_cargo {
            return None;
        }
        Some(Self {
            cwd: PathBuf::from(&request.cwd.0),
            cargo: program.clone(),
            env: request
                .env
                .iter()
                .map(|(name, value)| (name.0.clone(), value.0.clone()))
                .collect(),
        })
    }

    /// `program args` as the run would start it, its stdout captured.
    fn command(&self, program: &OsString, args: &[&str], dir: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    /// The workspace as `cargo metadata` describes it.
    fn workspace(&self) -> Option<Workspace> {
        let output = self
            .command(
                &self.cargo,
                &["metadata", "--format-version", "1", "--no-deps"],
                &self.cwd,
            )
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// The git common directory of the repository holding `dir`, if any.
    fn repository(&self, dir: &Path) -> Option<PathBuf> {
        let output = self
            .command(
                &OsString::from("git"),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                dir,
            )
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let common = Path::new(text.trim_end());
        Some(store::simplified(common))
    }
}
