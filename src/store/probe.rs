//! What cargo and git say about a workspace, asked as a run would ask:
//! with its cargo, in its directory, with its environment.
//!
//! - [`Probe::workspace`]: `cargo metadata --format-version 1 --no-deps`,
//!   which names the workspace root, its `target/` directory and its build
//!   directory (`build_directory`, cargo 1.91 and later; an older cargo's
//!   build directory is the target directory). It takes no lock.
//! - [`Probe::repository`]: the git common directory of the repository
//!   holding a directory, the same for every worktree of one repository.
//! - [`Probe::worktrees`]: the worktrees of a repository, as `git worktree
//!   list` names them.
//! - [`Probe::git_in`]: any other git command, as the run would run it.
//!
//! Only a program named `cargo` is ever started with `metadata`'s
//! arguments ([`Probe::of`]), never one standing in for cargo under another
//! name.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::protocol::RunRequest;
use crate::{environment, store};

/// A workspace as cargo places it.
#[derive(Debug, Deserialize)]
pub struct Workspace {
    #[serde(rename = "workspace_root")]
    pub root: PathBuf,
    #[serde(rename = "target_directory")]
    pub target: PathBuf,
    /// Absent from a cargo older than `build-dir`, whose build directory is
    /// the target directory.
    #[serde(rename = "build_directory")]
    pub build: Option<PathBuf>,
    #[serde(default)]
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    pub manifest_path: PathBuf,
    pub source: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
pub struct Dependency {
    pub path: Option<PathBuf>,
}

impl Workspace {
    /// Where cargo keeps its intermediates: the build directory, else the
    /// target directory.
    pub fn build_dir(&self) -> PathBuf {
        self.build.clone().unwrap_or_else(|| self.target.clone())
    }
}

/// What it takes to ask about a workspace as a run would.
pub struct Probe {
    pub cwd: PathBuf,
    cargo: OsString,
    env: Vec<(OsString, OsString)>,
    args: Vec<OsString>,
    target: Option<PathBuf>,
}

impl Probe {
    /// The probe for run `request`, if its program is named `cargo`.
    pub fn of(request: &RunRequest) -> Option<Self> {
        let program = &request.argv.first()?.0;
        let named_cargo = Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo"));
        if !named_cargo {
            return None;
        }
        Some(Self::request(request))
    }

    /// Cargo selected by a cargo-subcommand request, including wrappers.
    pub fn request(request: &RunRequest) -> Self {
        let cwd = request.cwd();
        let argv: Vec<_> = request
            .argv
            .iter()
            .skip(1)
            .map(|arg| arg.0.clone())
            .collect();
        let args = metadata_args(&argv, &cwd);
        let target = target_override(&argv, &cwd);
        Self {
            cwd,
            cargo: request
                .argv
                .first()
                .map(|program| program.0.clone())
                .unwrap_or_default(),
            env: request
                .env
                .iter()
                .map(|(name, value)| (name.0.clone(), value.0.clone()))
                .collect(),
            args,
            target,
        }
    }

    /// The probe for `cwd`, with this process's environment and its cargo
    /// (`CARGO`, else `cargo` on the search path).
    pub fn here(cwd: PathBuf) -> Self {
        let cargo = std::env::var_os("CARGO")
            .filter(|cargo| !cargo.is_empty())
            .unwrap_or_else(|| OsString::from("cargo"));
        let args = metadata_args(&[], &cwd);
        Self {
            cwd,
            cargo,
            env: std::env::vars_os().collect(),
            args,
            target: None,
        }
    }

    /// `program args` as the run would start it, its stdout captured.
    fn command(&self, program: &OsString, args: &[&str], dir: &Path) -> Command {
        let env = self.env.iter().map(|(name, value)| (name, value));
        environment::command(program, args, dir, env)
    }

    /// git with `args` in `dir`, taking no optional lock.
    fn git(&self, args: &[&str], dir: &Path) -> Option<String> {
        let output = self
            .command(&OsString::from("git"), args, dir)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()
    }

    /// git with `args` in `dir`, taking no optional lock, with `index` as
    /// its index file when given: its stdout, or why it failed.
    pub fn git_in(
        &self,
        dir: &Path,
        args: &[&str],
        index: Option<&Path>,
    ) -> Result<Vec<u8>, String> {
        let mut command = self.command(&OsString::from("git"), args, dir);
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stderr(Stdio::piped());
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        let output = command
            .output()
            .map_err(|error| format!("cannot run git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "`git {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    }

    /// The workspace as `cargo metadata` describes it.
    pub fn workspace(&self) -> Option<Workspace> {
        self.workspace_at(None)
    }

    /// The local dependency declarations of a selected manifest, without
    /// resolving or downloading registry dependencies.
    pub fn workspace_at(&self, manifest: Option<&Path>) -> Option<Workspace> {
        self.metadata(manifest, false)
    }

    /// Resolve local sources, including registry/git patches, without network
    /// access. All features expose optional path dependencies as well. Failure
    /// disables result caching; it must never certify an incomplete inventory.
    pub fn sources_at(&self, manifest: Option<&Path>) -> Option<Workspace> {
        self.metadata(manifest, true)
    }

    fn metadata(&self, manifest: Option<&Path>, sources: bool) -> Option<Workspace> {
        let mut args = self.args.clone();
        if sources {
            args.retain(|arg| arg != "--no-deps");
            args.push("--all-features".into());
            if !args.iter().any(|arg| arg == "--offline") {
                args.push("--offline".into());
            }
        }
        if let Some(manifest) = manifest {
            let mut index = 0;
            while index < args.len() {
                if args[index] == "--manifest-path" {
                    args.drain(index..index + 2);
                } else {
                    index += 1;
                }
            }
            args.extend([
                OsString::from("--manifest-path"),
                manifest.as_os_str().to_owned(),
            ]);
        }
        let mut command = environment::command(
            &self.cargo,
            args,
            &self.cwd,
            self.env.iter().map(|(name, value)| (name, value)),
        );
        if let Some(target) = &self.target {
            command
                .env("CARGO_TARGET_DIR", target)
                .env("CARGO_BUILD_TARGET_DIR", target);
        }
        let output = command.output().ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// The git common directory of the repository holding `dir`, if any.
    pub fn repository(&self, dir: &Path) -> Option<PathBuf> {
        let text = self.git(
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            dir,
        )?;
        Some(store::simplified(Path::new(text.trim_end())))
    }

    /// The worktrees of the repository whose git common directory is
    /// `repository`, as `git worktree list` lists them: the main worktree
    /// first. Bare repositories and worktrees git marks prunable (their
    /// directory is gone) are left out.
    pub fn worktrees(&self, repository: &Path) -> Vec<PathBuf> {
        let mut git_dir = OsString::from("--git-dir=");
        git_dir.push(repository);
        let Some(git_dir) = git_dir.to_str() else {
            return Vec::new();
        };
        let Some(text) = self.git(&[git_dir, "worktree", "list", "--porcelain"], &self.cwd) else {
            return Vec::new();
        };
        let mut worktrees = Vec::new();
        for entry in text.split("\n\n") {
            let mut path = None;
            let mut usable = true;
            for line in entry.lines() {
                if let Some(rest) = line.strip_prefix("worktree ") {
                    // git writes `/` between components on Windows too.
                    path = Some(Path::new(rest).components().collect::<PathBuf>());
                } else if line == "bare" || line.starts_with("prunable") {
                    usable = false;
                }
            }
            if let (Some(path), true) = (path, usable) {
                worktrees.push(path);
            }
        }
        worktrees
    }
}

/// Only options Cargo metadata understands. --target-dir has no metadata
/// flag; express its higher-priority value as a final config override.
fn metadata_args(args: &[OsString], cwd: &Path) -> Vec<OsString> {
    let mut output = Vec::new();
    if let Some(toolchain) = args
        .first()
        .filter(|arg| arg.to_string_lossy().starts_with('+'))
    {
        output.push(toolchain.clone());
    }
    output.extend(["metadata", "--format-version", "1", "--no-deps"].map(OsString::from));
    let mut target = None;
    let mut arguments = args.iter().peekable();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        let Some(text) = argument.to_str() else {
            continue;
        };
        let (name, joined) = text
            .split_once('=')
            .map_or((text, None), |(name, value)| (name, Some(value)));
        if ["--manifest-path", "-m", "--config", "-Z", "--target-dir"].contains(&name) {
            let value = joined
                .map(OsString::from)
                .or_else(|| arguments.next().cloned());
            if let Some(value) = value {
                if name == "--target-dir" {
                    target = Some(cwd.join(value));
                } else {
                    output.push(if name == "-m" {
                        "--manifest-path".into()
                    } else {
                        name.into()
                    });
                    output.push(value);
                }
            }
        } else if ["--locked", "--offline", "--frozen"].contains(&name) {
            output.push(argument.clone());
        } else if let Some(flag) = text.strip_prefix("-Z").filter(|flag| !flag.is_empty()) {
            output.extend([OsString::from("-Z"), OsString::from(flag)]);
        }
    }
    if let Some(target) = target.and_then(|path| path.to_str().map(str::to_owned)) {
        let quoted = serde_json::to_string(&target).expect("a path string serializes");
        output.extend([
            OsString::from("--config"),
            OsString::from(format!("build.target-dir={quoted}")),
        ]);
    }
    output
}

fn target_override(args: &[OsString], cwd: &Path) -> Option<PathBuf> {
    let mut target = None;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        if argument == "--target-dir" {
            target = arguments.next().map(|path| cwd.join(path));
        } else if let Some(path) = argument
            .to_str()
            .and_then(|text| text.strip_prefix("--target-dir="))
        {
            target = Some(cwd.join(path));
        }
    }
    target
}
