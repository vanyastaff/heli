//! Recycling (DESIGN.md §6.4): a worktree without a build directory gets
//! the most recently used orphan of its repository, renamed to the build
//! directory cargo names for it. In one store that is a rename: no copy,
//! no extra disk, and one build directory still belongs to one worktree.
//!
//! [`claim`] does it, once cargo has named the build directory
//! (`crate::store::probe`):
//!
//! 1. The build directory must be in a store heli manages, and must not
//!    exist: an existing one, even empty, is never replaced.
//! 2. The donor is the orphan in that store (`crate::store::State::Orphan`:
//!    its workspace's `Cargo.toml` is gone, and none of cargo's locks in it
//!    is held) whose owner manifest names the same repository, the one with
//!    the latest last run. Live, busy and unknown directories, orphans of
//!    other repositories or outside any, and what eviction moved into the
//!    store's `.trash/` (DESIGN.md §6.3), are never taken.
//! 3. The path scan runs on the donor where it lies, before the rename
//!    (`crate::store::scan`): every unit that compiled in a path under the
//!    donor's workspace or the donor's own path loses its fingerprint. So
//!    the build directory is whole the moment it appears under its new
//!    name, and a cargo that starts then never sees it half scanned.
//! 4. The rename. When it fails (another recycler or eviction took the
//!    donor first) the next orphan is tried, unless the build directory has
//!    appeared meanwhile.
//! 5. The owner manifest now names the new workspace, keeping its creation
//!    time.
//!
//! Two recyclers need no lock between them: a rename never replaces a
//! directory that has files, and each scans the donor under its old name,
//! which a rename by the other takes away.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::probe::Probe;
use super::registry::Registry;
use super::{Dir, Kind, State, owner};
use crate::config::StoreConfig;

/// A build directory that was recycled.
#[derive(Debug, Clone)]
pub struct Recycled {
    /// The donor's path, before the rename.
    pub from: PathBuf,
    /// The donor's workspace, which was removed.
    pub donor: PathBuf,
    /// The build directory's path now.
    pub to: PathBuf,
    /// The workspace it now belongs to.
    pub workspace: PathBuf,
    /// The fingerprints the path scan deleted.
    pub invalidated: usize,
}

impl Recycled {
    /// The fields of its `recycle` event (`crate::daemon::events`), which
    /// says what set it off: `trigger`.
    pub fn event(&self, trigger: &str) -> serde_json::Value {
        let text = |path: &Path| path.to_string_lossy().into_owned();
        json!({
            "trigger": trigger,
            "workspace": text(&self.workspace),
            "to": text(&self.to),
            "donor": text(&self.donor),
            "from": text(&self.from),
            "invalidated": self.invalidated,
        })
    }

    /// What a person reads of it.
    pub fn describe(&self) -> String {
        format!(
            "recycled the build directory of {}, a removed worktree; \
             the path scan invalidated {} units",
            self.donor.display(),
            self.invalidated
        )
    }
}

/// Gives the workspace `workspace` of `repository`, whose build directory
/// cargo names `build_dir`, the most recently used orphan of the same
/// repository, if `build_dir` does not exist yet and is in a store heli
/// manages; `at` is the run's start, for the owner manifest.
pub fn claim(
    config: &StoreConfig,
    build_dir: &Path,
    workspace: &Path,
    repository: &Path,
    at: u64,
) -> Result<Option<Recycled>, String> {
    let Some(store) = super::store_of(config, build_dir) else {
        return Ok(None);
    };
    if exists(build_dir) {
        return Ok(None);
    }
    for donor in orphans(&store, repository) {
        let Some(owner) = &donor.owner else {
            continue;
        };
        let old = [owner.workspace.clone(), donor.path.clone()];
        let invalidated = super::scan::invalidate(&donor.path, &old).len();
        if let Some(parent) = build_dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        if let Err(error) = std::fs::rename(&donor.path, build_dir) {
            if exists(build_dir) {
                return Ok(None);
            }
            if exists(&donor.path) {
                return Err(format!(
                    "cannot move {} to {}: {error}",
                    donor.path.display(),
                    build_dir.display()
                ));
            }
            // Taken by another recycler, or by eviction: the next one.
            continue;
        }
        if let Err(error) = owner::record_run(build_dir, workspace, Some(repository), at) {
            return Err(format!(
                "recycled {} to {}, but cannot write its owner manifest: {error}",
                donor.path.display(),
                build_dir.display()
            ));
        }
        return Ok(Some(Recycled {
            from: donor.path,
            donor: owner.workspace.clone(),
            to: build_dir.to_path_buf(),
            workspace: workspace.to_path_buf(),
            invalidated,
        }));
    }
    Ok(None)
}

/// Gives the worktree `worktree` a build directory, as [`claim`] does,
/// when cargo, asked through `probe` from the worktree's top, names a
/// workspace inside it; `registry` names the stores beside the configured
/// ones. While no store holds an orphan of the worktree's repository,
/// cargo is not even asked.
pub fn for_worktree(
    config: &StoreConfig,
    registry: &Registry,
    probe: &Probe,
    worktree: &Path,
    at: u64,
) -> Result<Option<Recycled>, String> {
    let Some(repository) = probe.repository(worktree) else {
        return Ok(None);
    };
    let mut stores = super::stores(config, registry);
    stores.extend(super::serving(config, worktree));
    if stores
        .iter()
        .all(|store| orphans(store, &repository).is_empty())
    {
        return Ok(None);
    }
    let Some(workspace) = probe.workspace() else {
        return Ok(None);
    };
    if !inside(&workspace.root, worktree) {
        return Ok(None);
    }
    claim(
        config,
        &workspace.build_dir(),
        &workspace.root,
        &repository,
        at,
    )
}

/// Whether `path` is `dir` or below it, both as they resolve on disk.
fn inside(path: &Path, dir: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(dir)) {
        (Ok(path), Ok(dir)) => path.starts_with(dir),
        _ => false,
    }
}

/// The orphans in `store` of `repository`, the most recently used first.
pub fn orphans(store: &Path, repository: &Path) -> Vec<Dir> {
    let mut orphans: Vec<Dir> = super::build_dirs(store)
        .into_iter()
        .filter_map(|path| {
            let owner = owner::read(&path)?;
            let theirs = owner.repository.as_deref()?;
            if !super::same_path(theirs, repository) {
                return None;
            }
            let dir = Dir {
                kind: Kind::Build,
                store: Some(store.to_path_buf()),
                workspace: Some(owner.workspace.clone()),
                owner: Some(owner),
                path,
            };
            (dir.state() == State::Orphan).then_some(dir)
        })
        .collect();
    orphans.sort_by_key(|dir| std::cmp::Reverse(dir.owner.as_ref().map(|owner| owner.last_run)));
    orphans
}

/// Whether anything is at `path`, a link included.
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
