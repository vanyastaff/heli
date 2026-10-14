//! The source-state record (DESIGN.md §6.4, "Precise"): `.heli/sources` in
//! a build directory, what the files of its worktree were when cargo last
//! built there, so that a worktree that recycles the directory can look to
//! cargo as that one did.
//!
//! ```json
//! {"taken_at":1791911112123456700,
//!  "files":{"app/src/main.rs":{"blob":"3b18e512dba79e4c8300dd08aeb37f8e728b8dad",
//!                              "size":423,"mtime":1791910980000000000}}}
//! ```
//!
//! - `taken_at`: when it was taken, before anything was read;
//! - `files`: every tracked or untracked file of the worktree (ignored ones
//!   are not), by its path from the worktree's top as git writes it: its
//!   blob id, its size and its modification time. Times are nanoseconds
//!   since 1970.
//!
//! **Writing** ([`write`]). Never while a build holds one of cargo's locks
//! in the build directory. The blob ids come from the snapshot machinery of
//! the result store (`crate::daemon::key::staged`): `git add -A` into a
//! scratch copy of the index, here listed with `git ls-files --stage`. The
//! sizes and times are read from the file system afterwards, so a file
//! changed meanwhile has a newer time than its blob, and only looks newer
//! to cargo. Symbolic links and submodules are left out. The file is
//! replaced whole.
//!
//! **Validity** ([`valid`]). A record counts only if it was taken after the
//! last build in the directory: `taken_at` is at least the newest
//! modification time of a file there. heli's own `.heli/` and rustc's
//! `incremental/` caches (in each profile directory, beside
//! `.fingerprint/`) are left out: no freshness decision reads them, and a
//! build that changes what cargo decides writes a fingerprint.
//!
//! **Use** ([`stage`], then [`Worktree::settle`]). Each file of the new
//! worktree whose blob id and size are the record's gets the recorded time,
//! if it was last written before heli staged the worktree. The size is
//! checked too because a blob id is of the content after git's filters: two
//! files that differ only in line endings can share one. Every other file
//! last written at the donor's last build ([`last_build`]) or before gets
//! the time now, so that cargo rebuilds what was built from another
//! content; the rest keep their times, newer than every artifact already.
//! A worktree with no record to use recycles only when every file of it is
//! newer than that build already ([`Worktree::newer_than`]).

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::probe::Probe;
use crate::daemon::key;

/// A source-state record, as `.heli/sources` holds it (see the module
/// docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub taken_at: u64,
    pub files: BTreeMap<String, Recorded>,
}

/// A file as the record holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recorded {
    pub blob: String,
    pub size: u64,
    pub mtime: u64,
}

/// The record's path in `build_dir`.
pub fn path(build_dir: &Path) -> PathBuf {
    build_dir.join(".heli").join("sources")
}

/// Writes the record of `build_dir` from `worktree`, asking git through
/// `probe`; `Ok(false)` when it is not written because a build holds one of
/// cargo's locks there, or `build_dir` does not exist.
pub fn write(build_dir: &Path, worktree: &Path, probe: &Probe) -> Result<bool, String> {
    let taken_at = nanos(SystemTime::now()).ok_or("the clock is before 1970")?;
    if !build_dir.is_dir() || super::busy(build_dir) {
        return Ok(false);
    }
    let dir = build_dir.join(".heli");
    let mut files = BTreeMap::new();
    for staged in staged_files(probe, worktree, &dir)? {
        let Ok(metadata) = std::fs::symlink_metadata(worktree.join(&staged.path)) else {
            continue;
        };
        let mtime = metadata.modified().ok().and_then(nanos);
        if let (true, Some(mtime)) = (metadata.is_file(), mtime) {
            let recorded = Recorded {
                blob: staged.blob,
                size: metadata.len(),
                mtime,
            };
            files.insert(staged.path, recorded);
        }
    }
    let text =
        serde_json::to_vec(&Record { taken_at, files }).map_err(|error| error.to_string())?;
    // A name of this write's own, so that two writers never share a file.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let temporary = dir.join(format!(
        "sources.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, path(build_dir)));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "cannot write {}: {error}",
            path(build_dir).display()
        ));
    }
    Ok(true)
}

/// The record of `build_dir`, if it has one that counts: taken after the
/// last build there.
pub fn valid(build_dir: &Path) -> Option<Record> {
    valid_after(build_dir, last_build(build_dir).ok()?)
}

/// The record of `build_dir`, if it has one taken at `newest` or later:
/// `newest` is [`last_build`]'s answer.
pub fn valid_after(build_dir: &Path, newest: u64) -> Option<Record> {
    let text = std::fs::read(path(build_dir)).ok()?;
    let record: Record = serde_json::from_slice(&text).ok()?;
    (record.taken_at >= newest).then_some(record)
}

/// When cargo last wrote in `build_dir`: the newest modification time of a
/// file there, in nanoseconds since 1970, leaving out heli's `.heli/` and
/// rustc's `incremental/` caches, which no freshness decision reads; 0 for
/// none. A file of a worktree last written at this time or before looks to
/// cargo as old as the artifacts built from it, or older.
pub fn last_build(build_dir: &Path) -> io::Result<u64> {
    newest(build_dir, true)
}

/// The newest modification time of a file in `dir`, leaving out heli's
/// `.heli/` at the `top` and the `incremental/` of each profile directory;
/// 0 for none. Links are not followed. An error, which may hide a newer
/// file, is one.
fn newest(dir: &Path, top: bool) -> io::Result<u64> {
    let profile = dir.join(".fingerprint").is_dir();
    let mut latest = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let skipped = (top && name == ".heli") || (profile && name == "incremental");
            if !skipped {
                latest = latest.max(newest(&entry.path(), false)?);
            }
        } else {
            let modified = entry.metadata()?.modified()?;
            latest = latest.max(nanos(modified).unwrap_or(0));
        }
    }
    Ok(latest)
}

/// The files of a worktree as heli staged them (with every change staged,
/// `key::staged`), and when it did.
pub struct Worktree {
    /// Taken before the staging: a file last written at this time or later
    /// may have changed since it was staged.
    staged_at: SystemTime,
    files: Vec<Staged>,
}

/// Stages `worktree`, asking git through `probe`; `scratch` holds the
/// scratch index.
pub fn stage(worktree: &Path, probe: &Probe, scratch: &Path) -> Result<Worktree, String> {
    let staged_at = SystemTime::now();
    let files = staged_files(probe, worktree, scratch)?;
    Ok(Worktree { staged_at, files })
}

/// What [`Worktree::settle`] did to the files of a worktree.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// The files that got the time a record holds for them.
    pub restored: usize,
    /// The files that needed a time newer than a build and did not get it:
    /// cargo may take what was built from another content for theirs.
    pub failed: usize,
}

impl Worktree {
    /// Whether every file of `worktree` staged here (and still there) was
    /// last written after `newest`, the last build of a build directory
    /// ([`last_build`]): cargo then finds every unit built from one of
    /// them stale. Links are not followed.
    pub fn newer_than(&self, worktree: &Path, newest: u64) -> bool {
        self.files.iter().all(|file| {
            match std::fs::symlink_metadata(worktree.join(&file.path)) {
                // Gone: cargo finds a unit built from it stale.
                Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                Err(_) => false,
                Ok(metadata) => {
                    !metadata.is_file()
                        || metadata
                            .modified()
                            .ok()
                            .and_then(nanos)
                            .is_some_and(|modified| modified > newest)
                }
            }
        })
    }

    /// Sets the times of the files of `worktree` staged here so that cargo,
    /// in a build directory whose last build was at `newest`
    /// ([`last_build`]), takes nothing built from another content for
    /// theirs:
    ///
    /// - a file whose blob id and size `record` holds, last written before
    ///   the staging, gets the time recorded with it: what was built from
    ///   it is fresh;
    /// - any other file last written at `newest` or before gets the time
    ///   now (and never an older one), after every build there: what was
    ///   built from it is stale;
    /// - any other file keeps its time, newer than every build already.
    ///
    /// A file is changed through a handle that, on Windows, lets nobody
    /// write to it meanwhile.
    pub fn settle(&self, worktree: &Path, record: &Record, newest: u64) -> Settled {
        let mut settled = Settled::default();
        let after_build = UNIX_EPOCH + Duration::from_nanos(newest.saturating_add(1));
        for staged in &self.files {
            let file = worktree.join(&staged.path);
            let recorded = record
                .files
                .get(&staged.path)
                .filter(|recorded| recorded.blob == staged.blob);
            match settle(&file, recorded, newest, self.staged_at, after_build) {
                Some(true) => settled.restored += 1,
                Some(false) => {}
                None => settled.failed += 1,
            }
        }
        settled
    }
}

/// Sets the time of `file` as [`Worktree::settle`] says: `recorded` is
/// what the record holds for its staged content, if anything. `Some(true)`
/// when it got the recorded time, `Some(false)` when it needs no other,
/// `None` when it needed a newer time than `newest` and did not get one.
fn settle(
    file: &Path,
    recorded: Option<&Recorded>,
    newest: u64,
    staged_at: SystemTime,
    after_build: SystemTime,
) -> Option<bool> {
    let older_than_build =
        |modified: Option<u64>| modified.is_none_or(|modified| modified <= newest);
    let Ok(handle) = open_for_times(file) else {
        // Gone, or not a file: nothing to set. Kept open by a writer: its
        // time had better be newer than the build already.
        return match std::fs::symlink_metadata(file) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(false),
            Ok(metadata) if !metadata.is_file() => Some(false),
            Ok(metadata) => {
                (!older_than_build(metadata.modified().ok().and_then(nanos))).then_some(false)
            }
            Err(_) => None,
        };
    };
    let Ok(metadata) = handle.metadata() else {
        return None;
    };
    if !metadata.is_file() {
        return Some(false);
    }
    let modified = metadata.modified().ok();
    if let Some(recorded) = recorded
        && metadata.len() == recorded.size
        && modified.is_some_and(|modified| modified < staged_at)
        && handle
            .set_modified(UNIX_EPOCH + Duration::from_nanos(recorded.mtime))
            .is_ok()
    {
        return Some(true);
    }
    if !older_than_build(modified.and_then(nanos)) {
        return Some(false);
    }
    let now = SystemTime::now().max(after_build);
    handle.set_modified(now).is_ok().then_some(false)
}

/// `file`, opened to read and set its times: on Windows, so that nobody
/// writes to it while it is open (and not at all if someone already is).
#[cfg(windows)]
fn open_for_times(file: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_WRITE_ATTRIBUTES,
    };
    File::options()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(file)
}

/// `file`, opened to read and set its times.
#[cfg(unix)]
fn open_for_times(file: &Path) -> io::Result<File> {
    File::open(file)
}

/// A file in a staged index: its path from the worktree's top, and its
/// blob id.
#[derive(Debug)]
struct Staged {
    path: String,
    blob: String,
}

/// The regular files of `worktree`, with every change staged
/// (`key::staged`), asking git through `probe`; `scratch` holds the scratch
/// index.
fn staged_files(probe: &Probe, worktree: &Path, scratch: &Path) -> Result<Vec<Staged>, String> {
    let git = |args: &[&str], index: Option<&Path>| probe.git_in(worktree, args, index);
    let listing = key::staged(&git, worktree, scratch, |index| {
        git(&["ls-files", "--stage", "-z"], Some(index))
    })?;
    Ok(listing
        .split(|&byte| byte == 0)
        .filter_map(|entry| {
            // `<mode> <blob> <stage>\t<path>`
            let entry = std::str::from_utf8(entry).ok()?;
            let (info, path) = entry.split_once('\t')?;
            let mut info = info.split(' ');
            let (mode, blob, stage) = (info.next()?, info.next()?, info.next()?);
            let regular = mode == "100644" || mode == "100755";
            (regular && stage == "0").then(|| Staged {
                path: path.to_owned(),
                blob: blob.to_owned(),
            })
        })
        .collect())
}

/// `time` in nanoseconds since 1970, if it is after 1970 and before 2554.
fn nanos(time: SystemTime) -> Option<u64> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}
