//! Recycling (DESIGN.md §6.4): a worktree without a build directory gets
//! the most recently used orphan of its repository, renamed to the build
//! directory cargo names for it. In one store that is a rename: no copy,
//! no extra disk, and one build directory still belongs to one worktree.
//!
//! [`claim`] does it, once cargo has named the build directory
//! (`crate::store::probe`):
//!
//! 1. The build directory must be in a store heli manages, and must not
//!    exist: an existing one, even empty, is never replaced. One recycler
//!    at a time recycles in a store: each holds the store's recycling lock
//!    ([`lock`], waiting at most [`LOCK_WAIT`] for it), and looks again
//!    once it has it, since the one before may have recycled for the same
//!    worktree (the hook, the look and a run can race for one). Without
//!    it, on Windows, a directory another recycler walks (to scan it)
//!    cannot be renamed, so two recyclers of one donor could each make the
//!    other's rename fail, and cargo then makes an empty build directory.
//! 2. The donor is the orphan in that store (`crate::store::State::Orphan`:
//!    its workspace's `Cargo.toml` is gone, and none of cargo's locks in it
//!    is held) whose owner manifest names the same repository, the one with
//!    the latest last run. Live, busy and unknown directories, orphans of
//!    other repositories or outside any, and what eviction moved into the
//!    store's `.trash/` (DESIGN.md §6.3), are never taken.
//! 3. The claim: the donor's owner manifest names the new workspace (which
//!    lives, so the donor is no orphan any more), keeping its creation
//!    time, before anything else. From here on no other recycler takes it,
//!    under its old name or, once renamed, its new one. A recycler that
//!    gives the donor up (below) gives it its manifest back, unless another
//!    recycler claimed it since or it moved; one that dies before the
//!    rename leaves it named after a live workspace, which recycling never
//!    takes and eviction does (DESIGN.md §6.3), but never a directory that
//!    names a removed worktree at a live one's place.
//! 4. What cargo would find: when the donor was last built in
//!    (`crate::store::record::last_build`), and its source-state record if
//!    it counts (taken after that build). The new worktree is staged
//!    (`crate::store::record::stage`); a worktree git cannot stage gets
//!    nothing. Without a record, recycling is **conservative**, and only
//!    for a worktree whose every file is newer than that build, as in a
//!    worktree just made: cargo then rebuilds every unit built from them.
//!    Otherwise (a worktree older than the build) the next orphan is tried:
//!    cargo would find units built from another content fresh.
//! 5. The path scan runs on the donor where it lies, before the rename
//!    (`crate::store::scan`): every unit that compiled in a path under the
//!    donor's workspace or the donor's own path loses its fingerprint. So
//!    the build directory is whole the moment it appears under its new
//!    name, and a cargo that starts then never sees it half scanned.
//! 6. The rename. When it fails (another recycler or eviction took the
//!    donor first, or it cannot be moved now) the next orphan is tried,
//!    unless the build directory has appeared meanwhile.
//! 7. The owner manifest names the new workspace again, in case another
//!    recycler claimed the donor too, after this one.
//! 8. **Precise** recycling, with a record: each file of the new worktree
//!    with the content the record holds gets the recorded time, so that
//!    cargo finds fresh what it found fresh in the donor, and each other
//!    file last written at the donor's last build or before gets the time
//!    now, so that cargo rebuilds what was built from another content
//!    (`crate::store::record::Worktree::settle`). Should a file not get its
//!    newer time, every unit's fingerprint goes. Only the recycler whose
//!    rename succeeded touches the worktree's files, with the record of the
//!    directory it moved.
//!
//! Beside the lock, which an older heli does not take, a rename never
//! replaces a directory that has files, each recycler scans the donor
//! under its old name, which a rename by another takes away, and a claimed
//! donor is no orphan. Each checks again, before each donor, that the
//! build directory does not exist.
//!
//! [`for_worktree`] is the way in for the `post-checkout` hook
//! (`crate::recycle`) and the daemon's look at a repository's worktrees
//! (`crate::daemon::accounts`): it asks cargo where a worktree builds, and
//! only while an orphan of its repository waits.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use super::probe::Probe;
use super::registry::Registry;
use super::{Dir, Kind, State, owner, record};
use crate::config::StoreConfig;
use crate::worktree;

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
    /// For precise recycling, the files of the new worktree that got the
    /// times the donor's source-state record holds; `None` for conservative
    /// recycling.
    pub restored: Option<usize>,
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
            "mode": if self.restored.is_some() { "precise" } else { "conservative" },
            "restored": self.restored,
        })
    }

    /// What a person reads of it.
    pub fn describe(&self) -> String {
        let precisely = match self.restored {
            Some(restored) => format!(", precisely ({restored} files got their recorded times)"),
            None => String::new(),
        };
        format!(
            "recycled the build directory of {}, a removed worktree{precisely}; \
             the path scan invalidated {} units",
            self.donor.display(),
            self.invalidated
        )
    }
}

/// Gives the workspace `workspace` of `repository`, whose build directory
/// cargo names `build_dir`, the most recently used orphan of the same
/// repository, if `build_dir` does not exist yet and is in a store heli
/// manages; `at` is the run's start, for the owner manifest. git is asked
/// about the workspace's worktree through `probe`.
pub fn claim(
    config: &StoreConfig,
    build_dir: &Path,
    workspace: &Path,
    repository: &Path,
    at: u64,
    probe: &Probe,
) -> Result<Option<Recycled>, String> {
    let Some(store) = super::store_of(config, build_dir) else {
        return Ok(None);
    };
    if exists(build_dir) {
        return Ok(None);
    }
    // One recycler at a time in a store: another one may be recycling for
    // the same worktree (the hook, the look, a run), and it gives the build
    // directory one, or gives its donor up, before this one looks.
    let Some(_recycling) = lock(&store)? else {
        return Ok(None);
    };
    if exists(build_dir) {
        return Ok(None);
    }
    let worktree = worktree::of(workspace);
    let mut failure = None;
    for donor in orphans(&store, repository) {
        // Another recycler may have renamed its donor into place since; a
        // directory that exists is never replaced.
        if exists(build_dir) {
            return Ok(None);
        }
        let Some(owner) = donor.owner.clone() else {
            continue;
        };
        // Claimed, the donor names the new workspace, which lives: no other
        // recycler takes it for an orphan from here on, under its old name
        // or its new one.
        let claimed = match Claim::take(&donor.path, owner, workspace, repository, at) {
            Ok(claimed) => claimed,
            Err(error) => {
                failure = Some(format!("cannot claim {}: {error}", donor.path.display()));
                continue;
            }
        };
        // What cargo would find, read before the scan changes the directory.
        let Ok(newest) = record::last_build(&donor.path) else {
            failure = Some(format!(
                "cannot tell when {} was last built in",
                donor.path.display()
            ));
            continue;
        };
        let record = record::valid_after(&donor.path, newest);
        let files = record::stage(&worktree, probe, &donor.path.join(".heli"))
            .map_err(|error| format!("cannot stage {}: {error}", worktree.display()))?;
        // Without a record, the worktree's files must be newer than every
        // artifact already (as in a worktree just made): nothing tells
        // which of them the donor built from the same content.
        if record.is_none() && !files.newer_than(&worktree, newest) {
            continue;
        }
        let old = [claimed.donor().workspace.clone(), donor.path.clone()];
        let mut invalidated = super::scan::invalidate(&donor.path, &old).len();
        if let Some(parent) = build_dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        if let Err(error) = std::fs::rename(&donor.path, build_dir) {
            if exists(build_dir) {
                return Ok(None);
            }
            // Gone: taken by another recycler, or by eviction. Still there:
            // it cannot be moved now. Either way, the next one.
            if exists(&donor.path) {
                failure = Some(format!(
                    "cannot move {} to {}: {error}",
                    donor.path.display(),
                    build_dir.display()
                ));
            }
            continue;
        }
        let donor_workspace = claimed.kept();
        // Named so already, unless another recycler claimed the donor too
        // and wrote after this one: the directory then names its workspace,
        // which lives, until this.
        if let Err(error) = owner::record_run(build_dir, workspace, Some(repository), at) {
            return Err(format!(
                "recycled {} to {}, but cannot write its owner manifest: {error}",
                donor.path.display(),
                build_dir.display()
            ));
        }
        let restored = record.map(|record| {
            let settled = files.settle(&worktree, &record, newest);
            if settled.failed > 0 {
                // Some file still looks older than what was built from
                // another content: cargo rebuilds every unit instead.
                invalidated += super::scan::invalidate_all(build_dir).len();
            }
            settled.restored
        });
        return Ok(Some(Recycled {
            from: donor.path,
            donor: donor_workspace,
            to: build_dir.to_path_buf(),
            workspace: workspace.to_path_buf(),
            invalidated,
            restored,
        }));
    }
    failure.map_or(Ok(None), Err)
}

/// The longest a recycler waits for the store's recycling lock.
const LOCK_WAIT: Duration = Duration::from_secs(25);

/// The recycling lock of `store`, `.recycle.lock` in it, taken within
/// [`LOCK_WAIT`]: held while a recycler recycles, released when the file
/// closes (also when its process dies). `None` for a store that does not
/// exist, which holds no orphan; an error when another recycler held the
/// lock all that time, or it cannot be taken.
fn lock(store: &Path) -> Result<Option<File>, String> {
    let path = store.join(".recycle.lock");
    let file = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(_) if !store.is_dir() => return Ok(None),
        Err(error) => return Err(format!("cannot open {}: {error}", path.display())),
    };
    let waiting = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) if waiting.elapsed() < LOCK_WAIT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!(
                    "another recycler held {} for {} s",
                    path.display(),
                    LOCK_WAIT.as_secs()
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(format!("cannot lock {}: {error}", path.display()));
            }
        }
    }
}

/// A donor claimed for a new workspace: its owner manifest names that
/// workspace, which lives, so no other recycler takes it for an orphan.
/// Dropped before [`Claim::kept`], it gives the donor back its manifest, if
/// the donor is still where it was and no other recycler claimed it since.
struct Claim<'a> {
    path: &'a Path,
    donor: owner::Owner,
    claimed_for: &'a Path,
    kept: bool,
}

impl<'a> Claim<'a> {
    /// Claims the donor at `path`, whose manifest is `donor`, for
    /// `workspace` of `repository`, as a run that started `at`.
    fn take(
        path: &'a Path,
        donor: owner::Owner,
        workspace: &'a Path,
        repository: &Path,
        at: u64,
    ) -> std::io::Result<Self> {
        let claimed = owner::Owner {
            workspace: workspace.to_path_buf(),
            repository: Some(repository.to_path_buf()),
            created_at: donor.created_at,
            last_run: at,
        };
        owner::write(path, &claimed)?;
        Ok(Self {
            path,
            donor,
            claimed_for: workspace,
            kept: false,
        })
    }

    /// The donor's manifest, as it was.
    fn donor(&self) -> &owner::Owner {
        &self.donor
    }

    /// Keeps the claim: the donor was renamed into place. Returns the
    /// donor's workspace.
    fn kept(mut self) -> PathBuf {
        self.kept = true;
        self.donor.workspace.clone()
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let still_ours = owner::read(self.path)
            .is_some_and(|owner| super::same_path(&owner.workspace, self.claimed_for));
        if !self.kept && still_ours {
            let _ = owner::write(self.path, &self.donor);
        }
    }
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
        probe,
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
