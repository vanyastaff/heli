//! The runs in flight: what `status` lists, and the source of run ids.
//!
//! A run is admitted before anything starts and leaves when it has finished.
//!
//! **Run ids** are `r<number>`, unique for the life of the state directory
//! ([`RunIds`]): run logs and markers are keyed by them, and on Windows
//! every logon session has a daemon of its own on the same state directory.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use crate::status::{self, Phase};
use crate::{clock, runlog};

/// A run's id, `r<number>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunId(u64);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// What the registry knows of a run.
struct Entry {
    worktree: PathBuf,
    argv: Vec<OsString>,
    phase: Phase,
    started_at: SystemTime,
}

/// Numbers runs for every daemon of one state directory.
///
/// The next number is the counter file's, read and advanced for each run
/// under a file lock, so two daemons never give the same id. It is never
/// below one past the highest record in the run log, so a counter that is
/// lost or behind costs no record. A number that cannot be written back is
/// not given at all.
pub struct RunIds {
    counter: PathBuf,
    lock: PathBuf,
    records: PathBuf,
}

impl RunIds {
    /// Ids from the counter file `counter`, whose lock is the file `lock`,
    /// above the records in `records` (`crate::runlog`).
    pub fn new(counter: PathBuf, lock: PathBuf, records: PathBuf) -> Self {
        Self {
            counter,
            lock,
            records,
        }
    }

    /// A new id.
    pub fn next(&self) -> io::Result<RunId> {
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock)?;
        // Released when `lock` is closed.
        lock.lock()?;
        let stored = std::fs::read_to_string(&self.counter)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(1);
        let number = stored.max(highest_record(&self.records) + 1);
        write_atomically(&self.counter, &(number + 1).to_string())?;
        Ok(RunId(number))
    }
}

/// The highest run number with a record in `records`; 0 for none.
fn highest_record(records: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(records) else {
        return 0;
    };
    entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            runlog::run_number(name.to_str()?.strip_suffix(".log")?)
        })
        .max()
        .unwrap_or(0)
}

pub struct Registry {
    ids: RunIds,
    inner: Mutex<Inner>,
}

struct Inner {
    runs: BTreeMap<RunId, Entry>,
}

impl Registry {
    /// A registry whose runs `ids` numbers.
    pub fn new(ids: RunIds) -> Self {
        Self {
            ids,
            inner: Mutex::new(Inner {
                runs: BTreeMap::new(),
            }),
        }
    }

    /// Admits a run of `argv` in `worktree`, and returns its new id; the
    /// error says why it has none.
    pub fn admit(&self, worktree: PathBuf, argv: Vec<OsString>) -> io::Result<RunId> {
        let mut inner = self.lock();
        let id = self.ids.next()?;
        inner.runs.insert(
            id,
            Entry {
                worktree,
                argv,
                phase: Phase::Running,
                started_at: SystemTime::now(),
            },
        );
        Ok(id)
    }

    /// Removes a finished run.
    pub fn finish(&self, id: RunId) {
        self.lock().runs.remove(&id);
    }

    /// The runs in flight, oldest first.
    pub fn snapshot(&self) -> Vec<status::Run> {
        self.lock()
            .runs
            .iter()
            .map(|(id, entry)| status::Run {
                id: id.to_string(),
                worktree: entry.worktree.to_string_lossy().into_owned(),
                argv: entry
                    .argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
                phase: entry.phase,
                started_at: clock::unix_seconds(entry.started_at),
            })
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Replaces `path` with `contents`, never leaving it half-written.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path)
}
