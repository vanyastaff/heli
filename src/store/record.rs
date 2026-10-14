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
//! **Use** ([`restore`]). Each file of the new worktree whose blob id and
//! size are the record's gets the recorded time. The size is checked too
//! because a blob id is of the content after git's filters: two files that
//! differ only in line endings can share one. A file is changed only if it
//! was last written before heli staged the worktree, through a handle that,
//! on Windows, lets nobody write to it meanwhile; any other file keeps its
//! time, which is newer than every artifact.

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
    let text = std::fs::read(path(build_dir)).ok()?;
    let record: Record = serde_json::from_slice(&text).ok()?;
    let newest = newest(build_dir, true).ok()?;
    (record.taken_at >= newest).then_some(record)
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

/// Gives each file of `worktree` whose blob id and size `record` holds the
/// time recorded with it, asking git through `probe`; `scratch` is a
/// directory for scratch files. Returns how many files got it.
pub fn restore(
    record: &Record,
    worktree: &Path,
    probe: &Probe,
    scratch: &Path,
) -> Result<usize, String> {
    let staging = SystemTime::now();
    let mut restored = 0;
    for staged in staged_files(probe, worktree, scratch)? {
        let Some(recorded) = record.files.get(&staged.path) else {
            continue;
        };
        if recorded.blob == staged.blob && set_time(&worktree.join(&staged.path), recorded, staging)
        {
            restored += 1;
        }
    }
    Ok(restored)
}

/// Sets the modification time of `file` to `recorded`'s, if it has
/// `recorded`'s size and was last written before `staging`; whether it
/// did.
fn set_time(file: &Path, recorded: &Recorded, staging: SystemTime) -> bool {
    let Ok(file) = open_for_times(file) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    let unchanged = metadata.is_file()
        && metadata.len() == recorded.size
        && metadata.modified().is_ok_and(|modified| modified < staging);
    unchanged
        && file
            .set_modified(UNIX_EPOCH + Duration::from_nanos(recorded.mtime))
            .is_ok()
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
