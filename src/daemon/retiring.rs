//! Retiring daemons (docs/daemon.md, "Handover", #126): a daemon asked to
//! retire while it has runs in flight hands the endpoint and the state
//! directory's lock over at once, and serves its runs to their end. A new
//! daemon serves every new run meanwhile.
//!
//! What the two share is in the state directory, in `retiring/`
//! (`retiring-<session>/` on Windows):
//!
//! - `<pid>.json`, the retiring daemon's [`Record`]: its version, since
//!   when it retires, and its runs in flight, rewritten as they end;
//! - `<pid>.lock`, which it holds locked for as long as it lives, so that a
//!   record whose daemon is gone (killed, crashed: its pid may be another
//!   process's by now) is told apart and removed ([`live`]);
//! - `<pid>.cancel-<run>`, which another daemon creates to cancel one of its
//!   runs (`cargo heli cancel`, [`ask_cancel`]); the retiring daemon looks
//!   for them every [`LOOK`] and removes each as it acts on it.
//!
//! The retiring daemon removes all three as it exits.

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::status;

/// How often a retiring daemon looks for cancel requests.
pub const LOOK: Duration = Duration::from_millis(200);

/// What a retiring daemon tells of itself: what `status` shows of it.
pub type Record = status::Retiring;

/// This daemon, retiring: its record, and the lock that says it lives.
pub struct Handover {
    dir: PathBuf,
    pid: u32,
    /// Held locked until the daemon exits.
    _lock: File,
}

impl Handover {
    /// Starts retiring in `dir`, with `record`.
    pub fn begin(dir: &Path, record: &Record) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let pid = record.pid;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{pid}.lock")))?;
        lock.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::other("the retiring lock is held"),
            TryLockError::Error(error) => error,
        })?;
        let handover = Self {
            dir: dir.to_path_buf(),
            pid,
            _lock: lock,
        };
        handover.update(record)?;
        Ok(handover)
    }

    /// Rewrites the record.
    pub fn update(&self, record: &Record) -> io::Result<()> {
        let json = serde_json::to_vec(record).map_err(io::Error::other)?;
        crate::atomic::write(&self.dir.join(format!("{}.json", self.pid)), &json)
    }

    /// The runs another daemon asked to cancel since the last look; each
    /// request is removed.
    pub fn cancels(&self) -> Vec<String> {
        let prefix = format!("{}.cancel-", self.pid);
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut runs = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(run) = name.strip_prefix(&prefix) {
                let _ = std::fs::remove_file(entry.path());
                runs.push(run.to_owned());
            }
        }
        runs
    }

    /// Removes the record, its lock and any cancel request left: the
    /// daemon is about to exit.
    pub fn end(&self) {
        remove(&self.dir, self.pid);
    }
}

/// The records of the retiring daemons that still live, in `dir`; those
/// whose daemon is gone are removed.
pub fn live(dir: &Path) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_suffix(".json")
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if !lives(dir, pid) {
            remove(dir, pid);
            continue;
        }
        if let Ok(bytes) = std::fs::read(entry.path())
            && let Ok(record) = serde_json::from_slice::<Record>(&bytes)
        {
            records.push(record);
        }
    }
    records.sort_by_key(|record| record.since);
    records
}

/// Asks retiring daemon `pid` in `dir` to cancel `run`.
pub fn ask_cancel(dir: &Path, pid: u32, run: &str) -> io::Result<()> {
    File::create(dir.join(format!("{pid}.cancel-{run}"))).map(drop)
}

/// Whether retiring daemon `pid` lives: its lock is held.
fn lives(dir: &Path, pid: u32) -> bool {
    let Ok(lock) = File::options()
        .write(true)
        .open(dir.join(format!("{pid}.lock")))
    else {
        return false;
    };
    match lock.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => true,
    }
}

/// Removes the files of retiring daemon `pid` in `dir`.
fn remove(dir: &Path, pid: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let stem = format!("{pid}.");
    for entry in entries.filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().starts_with(&stem) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Handover, Record, ask_cancel, live};

    fn record(pid: u32) -> Record {
        Record {
            pid,
            version: "0.0.6".to_owned(),
            since: 10,
            runs: Vec::new(),
        }
    }

    /// A retiring daemon's record is live while it holds its lock; a cancel
    /// request reaches it once; a record left behind is removed (#126).
    #[test]
    fn a_record_lives_with_its_daemon() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let handover = Handover::begin(dir.path(), &record(42)).expect("begin");
        let found = live(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 42);

        ask_cancel(dir.path(), 42, "r7").expect("ask");
        assert_eq!(handover.cancels(), ["r7"]);
        assert!(handover.cancels().is_empty());

        // A record whose daemon is gone: nobody holds its lock.
        std::fs::write(
            dir.path().join("43.json"),
            serde_json::to_vec(&record(43)).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("43.lock"), b"").unwrap();
        assert_eq!(live(dir.path()).len(), 1);
        assert!(!dir.path().join("43.json").exists());

        handover.end();
        drop(handover);
        assert!(live(dir.path()).is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
