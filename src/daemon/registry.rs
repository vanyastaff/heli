//! The runs in flight: what `status` lists, and the source of run ids.
//!
//! A run is admitted before anything starts and leaves when it has finished.
//! Ids are `r<number>`; the next number is kept in the state directory, so a
//! restarted daemon never reuses an id (run logs and markers are keyed by
//! it).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use crate::clock;
use crate::status::{self, Phase};

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

pub struct Registry {
    counter_file: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    next: u64,
    runs: BTreeMap<RunId, Entry>,
}

impl Registry {
    /// A registry whose ids continue from `counter_file`.
    pub fn new(counter_file: &Path) -> Self {
        let next = std::fs::read_to_string(counter_file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(1);
        Self {
            counter_file: counter_file.to_path_buf(),
            inner: Mutex::new(Inner {
                next,
                runs: BTreeMap::new(),
            }),
        }
    }

    /// Admits a run of `argv` in `worktree`, and returns its new id.
    pub fn admit(&self, worktree: PathBuf, argv: Vec<OsString>) -> RunId {
        let mut inner = self.lock();
        let id = RunId(inner.next);
        inner.next += 1;
        // Written while locked, so ids never go backwards on disk. A failed
        // write costs only uniqueness across a restart.
        let _ = write_atomically(&self.counter_file, &inner.next.to_string());
        inner.runs.insert(
            id,
            Entry {
                worktree,
                argv,
                phase: Phase::Running,
                started_at: SystemTime::now(),
            },
        );
        id
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
                slots: 0,
                share: 0.0,
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
