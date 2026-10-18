//! The registered `target/` directories, kept across daemon restarts in
//! `store.json` in the state directory:
//!
//! ```json
//! {"targets":[{"path":"D:\\flui\\target","workspace":"D:\\flui","shared":false}]}
//! ```
//!
//! A `target/` directory seen for two workspaces is `shared` from then on:
//! a global `CARGO_TARGET_DIR` or `build.target-dir`. The build store then
//! leaves it alone (`super::kind`, #65).
//!
//! A `target/` directory whose workspace builds its intermediates elsewhere
//! (`build.build-dir`) has that `build_dir`, and `separated_since`: when
//! heli first saw them apart. What its profiles still hold of cargo's
//! intermediates, untouched since then, is superseded (`super::superseded`,
//! #92). Seen building in the target directory again, it loses both.
//!
//! A `target/` directory is registered, with the workspace it was last seen
//! for, when a heli run builds in that workspace (`crate::daemon::accounts`).
//! The daemon adds to it; `gc --dry-run` and the daemon's accounts read it.
//! It is replaced whole, so a reader never sees half of it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::same_path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    pub targets: Vec<Target>,
}

/// A registered `target/` directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub path: PathBuf,
    /// The workspace it was last seen for.
    pub workspace: PathBuf,
    /// It was seen for more than one workspace.
    #[serde(default)]
    pub shared: bool,
    /// Where its workspace builds its intermediates, when not here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_dir: Option<PathBuf>,
    /// When heli first saw its workspace build them elsewhere, in seconds
    /// since 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separated_since: Option<u64>,
}

impl Target {
    /// `path`, seen at `at` for `workspace`, which builds its intermediates
    /// in `build_dir`.
    pub fn seen(path: PathBuf, workspace: PathBuf, build_dir: &Path, at: u64) -> Self {
        let apart = !same_path(build_dir, &path);
        Self {
            path,
            workspace,
            shared: false,
            build_dir: apart.then(|| build_dir.to_path_buf()),
            separated_since: apart.then_some(at),
        }
    }
}

/// The registry's file in `state_dir`.
fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("store.json")
}

impl Registry {
    /// The registry in `state_dir`; none there, or one that cannot be read,
    /// is empty.
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read(path(state_dir))
            .ok()
            .and_then(|text| serde_json::from_slice(&text).ok())
            .unwrap_or_default()
    }

    /// Registers `target` in the registry in `state_dir`, or updates its
    /// workspace and build directory. A target directory already apart from
    /// its build directory keeps the time it was first seen apart. Only one
    /// writer may call this at a time.
    pub fn add(state_dir: &Path, target: Target) -> io::Result<()> {
        let mut registry = Self::load(state_dir);
        if !registry.merge(target) {
            return Ok(());
        }
        registry.save(state_dir)
    }

    /// Merges `target` into the registry; whether that changed it.
    fn merge(&mut self, target: Target) -> bool {
        let known = self
            .targets
            .iter_mut()
            .find(|known| same_path(&known.path, &target.path));
        let Some(known) = known else {
            self.targets.push(target);
            return true;
        };
        let before = known.clone();
        if !same_path(&known.workspace, &target.workspace) {
            known.workspace = target.workspace;
            known.shared = true;
        }
        known.separated_since = target
            .separated_since
            .map(|at| known.separated_since.unwrap_or(at));
        known.build_dir = target.build_dir;
        *known != before
    }

    fn save(&self, state_dir: &Path) -> io::Result<()> {
        static WRITES: AtomicU64 = AtomicU64::new(0);
        let text = serde_json::to_vec(self).map_err(io::Error::other)?;
        let temporary = state_dir.join(format!(
            "store.json.{}.{}.tmp",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path(state_dir)).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(build_dir: &str, at: u64) -> Target {
        Target::seen(
            PathBuf::from("/w/target"),
            PathBuf::from("/w"),
            Path::new(build_dir),
            at,
        )
    }

    #[test]
    fn a_target_keeps_when_it_was_first_seen_apart_from_its_build_directory() {
        let mut registry = Registry::default();
        assert!(registry.merge(seen("/w/target", 10)));
        assert_eq!(registry.targets[0].separated_since, None);
        assert!(!registry.merge(seen("/w/target", 20)), "nothing changed");

        assert!(registry.merge(seen("/store/ab/1", 30)));
        assert_eq!(registry.targets[0].separated_since, Some(30));
        assert!(!registry.merge(seen("/store/ab/1", 40)), "still since 30");
        assert!(registry.merge(seen("/store/cd/2", 50)));
        assert_eq!(registry.targets[0].separated_since, Some(30));
        assert_eq!(
            registry.targets[0].build_dir,
            Some(PathBuf::from("/store/cd/2"))
        );

        assert!(registry.merge(seen("/w/target", 60)), "back together");
        assert_eq!(registry.targets[0].separated_since, None);
        assert_eq!(registry.targets[0].build_dir, None);
    }

    #[test]
    fn an_old_registry_still_loads() {
        let registry: Registry = serde_json::from_str(
            r#"{"targets":[{"path":"/w/target","workspace":"/w","shared":false}]}"#,
        )
        .expect("an old registry");
        assert_eq!(registry.targets[0].separated_since, None);
    }
}
