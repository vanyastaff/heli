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
    /// Local crates that `[patch]` or `[replace]` put in place of
    /// dependencies, when cargo did not resolve them as packages itself
    /// ([`Probe::sources_at`]'s fallback): each one's directory.
    #[serde(skip)]
    pub patches: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct Package {
    #[serde(default)]
    pub name: String,
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
    epoch: Option<PathBuf>,
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
            epoch: request
                .dependency_epoch
                .as_ref()
                .map(|path| PathBuf::from(&path.0)),
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
            // Hooks and a daemon started by nested Heli inherit the same
            // selected context as Cargo. Preserve that existing selection
            // when their own metadata query needs an independent guardian.
            epoch: std::env::var_os("HELI_DEPENDENCY_EPOCH")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
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

    /// The probe for `cwd`, as this process's environment would run
    /// `cargo <args>` with `cargo`: its toolchain, manifest and
    /// configuration.
    pub fn cargo_in(cwd: PathBuf, cargo: OsString, args: &[OsString]) -> Self {
        let args_for_metadata = metadata_args(args, &cwd);
        let target = target_override(args, &cwd);
        Self {
            args: args_for_metadata,
            target,
            cargo,
            ..Self::here(cwd)
        }
    }

    /// The workspace as `cargo metadata` describes it.
    pub fn workspace(&self) -> Option<Workspace> {
        self.workspace_at(None)
    }

    /// The workspace's members as `cargo metadata --no-deps` describes
    /// them, or why cargo could not say.
    pub fn members(&self) -> Result<Workspace, String> {
        self.metadata(None, false)
    }

    /// The local dependency declarations of a selected manifest, without
    /// resolving or downloading registry dependencies.
    pub fn workspace_at(&self, manifest: Option<&Path>) -> Option<Workspace> {
        self.metadata(manifest, false).ok()
    }

    /// The local sources of a workspace, without network access: its
    /// packages that have no registry or git source, each with its declared
    /// path dependencies, and the local crates its patches put in place.
    /// Failure disables result caching; it must never certify an incomplete
    /// inventory.
    ///
    /// First a resolution of every feature for every platform, which also
    /// names the packages patches select. It needs every package of the
    /// lockfile in the Cargo home, which a private home (`[cargo] home =
    /// "isolated"`) lacks for optional and other-platform dependencies it
    /// never built (#66). Then, should that fail, the manifests alone
    /// (`--no-deps`): they declare every path dependency, optional and
    /// platform-specific ones included, and [`local_patches`] reads the
    /// patches. Registry and git sources are in the lockfile, which is in
    /// the tree. The error is the fallback's, with cargo's own first error.
    pub fn sources_at(&self, manifest: Option<&Path>) -> Result<Workspace, String> {
        let resolved = match self.metadata(manifest, true) {
            Ok(workspace) => return Ok(workspace),
            Err(why) => why,
        };
        let mut workspace = self
            .metadata(manifest, false)
            .map_err(|why| format!("{why} (resolving every feature: {resolved})"))?;
        workspace.patches = local_patches(&workspace.root, &self.cwd, self.cargo_home().as_deref());
        Ok(workspace)
    }

    /// The Cargo home as cargo finds it for the run: `CARGO_HOME` (relative
    /// to the working directory), else `.cargo` in the user's home.
    fn cargo_home(&self) -> Option<PathBuf> {
        let var = |name: &str| {
            self.env
                .iter()
                .rev()
                .find(|(candidate, _)| environment::same_name(candidate, name))
                .map(|(_, value)| value)
                .filter(|value| !value.is_empty())
        };
        if let Some(home) = var("CARGO_HOME") {
            return Some(self.cwd.join(home));
        }
        let user = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        var(user).map(|home| Path::new(home).join(".cargo"))
    }

    fn metadata(&self, manifest: Option<&Path>, sources: bool) -> Result<Workspace, String> {
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
        // Metadata is an independent consumer: its parent daemon can die
        // while Cargo still runs. A worker-thread lease cannot protect that
        // orphan, so let a guardian own the epoch through the actual query.
        let output = match &self.epoch {
            Some(epoch) => super::runner::capture(command, epoch),
            None => command.output(),
        }
        .map_err(|error| format!("cannot run cargo metadata: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first = stderr
                .lines()
                .find(|line| line.trim_start().starts_with("error"))
                .or_else(|| stderr.lines().find(|line| !line.trim().is_empty()))
                .unwrap_or("no message");
            return Err(format!("cargo metadata failed: {}", first.trim()));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("cargo metadata printed no workspace: {error}"))
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

/// The directories of the local crates that patches of the workspace at
/// `root` put in place, as cargo reads them: `[patch.<source>.<name>]` and
/// `[replace."<spec>"]` with a `path` in its root manifest (relative to the
/// root), and `[patch...]` with a `path` in the Cargo configuration of
/// `cwd` and every directory above it, then of the Cargo home (relative to
/// the directory holding that `.cargo/`, or the home itself). An unreadable
/// file adds nothing: cargo, which read it to list the workspace, would have
/// failed on it.
fn local_patches(root: &Path, cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    fn paths(table: Option<&toml::Value>, base: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Some(table) = table.and_then(toml::Value::as_table) else {
            return;
        };
        for value in table.values() {
            match (depth, value.get("path").and_then(toml::Value::as_str)) {
                (0, Some(path)) => found.push(base.join(path)),
                (0, None) => {}
                _ => paths(Some(value), base, depth - 1, found),
            }
        }
    }
    let read = |file: &Path| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .map(toml::Value::Table)
    };
    let mut found = Vec::new();
    if let Some(manifest) = read(&root.join("Cargo.toml")) {
        paths(manifest.get("patch"), root, 1, &mut found);
        paths(manifest.get("replace"), root, 0, &mut found);
    }
    let configs = cwd
        .ancestors()
        .map(|dir| (dir.join(".cargo"), dir.to_path_buf()))
        .chain(home.map(|home| (home.to_path_buf(), home.to_path_buf())));
    for (dir, base) in configs {
        for name in ["config.toml", "config"] {
            if let Some(config) = read(&dir.join(name)) {
                paths(config.get("patch"), &base, 1, &mut found);
                break;
            }
        }
    }
    found.sort();
    found.dedup();
    found
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

#[cfg(test)]
mod tests {
    use super::local_patches;

    /// Patches with a path, in the root manifest (relative to the root) and
    /// in a Cargo configuration (relative to the directory holding its
    /// `.cargo/`); patches without one are not local.
    #[test]
    fn local_patches_come_from_the_manifest_and_the_configuration() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join(".cargo")).expect("dirs");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"ws\"\n\n\
             [patch.crates-io]\nlocal = { path = \"vendor/local\" }\nremote = { git = \"https://x\" }\n\n\
             [replace]\n\"old:1.0.0\" = { path = \"old\" }\n",
        )
        .expect("manifest");
        std::fs::write(
            root.join(".cargo").join("config.toml"),
            "[patch.'https://github.com/a/b']\nfrom-git = { path = \"../fork\" }\n",
        )
        .expect("config");
        let found = local_patches(&root, &root, None);
        assert_eq!(
            found,
            [
                root.join("../fork"),
                root.join("old"),
                root.join("vendor/local")
            ]
        );
    }
}
