//! The runs in flight: what `status` lists, the source of run ids, and the
//! worktree lock.
//!
//! A run is admitted before anything starts and leaves when it has finished.
//! Ids are `r<number>`; the next number is kept in the state directory, so a
//! restarted daemon never reuses an id (run logs and markers are keyed by
//! it).
//!
//! **The worktree lock** (`DESIGN.md` §5). A run holds its worktree's lock
//! shared when it reads the sources and exclusive when it rewrites them, as
//! its class says (`Class::worktree_lock`). Turns follow admission order: a
//! run waits ([`Registry::wait_for_worktree`]) until no run admitted before
//! it in the same worktree conflicts with it, and two runs conflict when
//! either is exclusive. So a mutate run waits for every earlier run in its
//! worktree, every later one waits for it, and a mutate run that waits is
//! never overtaken by runs that would keep it waiting. A run holds the lock
//! from the end of its wait until it finishes. The worktree is
//! `crate::worktree::of` the run's directory, compared as a canonical path,
//! so two spellings of one directory (on Windows, in another case) share
//! one lock.
//!
//! A nested run, asked for from inside another heli run, never waits: the
//! run around it may hold the lock, and would then wait for it forever
//! (`cargo heli exec -- cargo heli fmt`). It still takes its turn, so later
//! runs wait for it. Since only runs that are not nested wait, and only for
//! runs admitted before them, no wait can close a cycle.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use crate::class::{Access, Class};
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
    /// The worktree as the lock tells worktrees apart: its canonical path.
    lock: PathBuf,
    /// Asked for from inside another heli run: it does not wait.
    nested: bool,
    argv: Vec<OsString>,
    class: Class,
    phase: Phase,
    started_at: SystemTime,
}

pub struct Registry {
    counter_file: PathBuf,
    inner: Mutex<Inner>,
    /// Signalled when a run finishes, for the runs waiting for its worktree.
    finished: Condvar,
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
            finished: Condvar::new(),
        }
    }

    /// Admits a run of `argv`, of `class`, in `worktree`, and returns its new
    /// id. A `nested` run was asked for from inside another heli run.
    pub fn admit(
        &self,
        worktree: PathBuf,
        argv: Vec<OsString>,
        class: Class,
        nested: bool,
    ) -> RunId {
        let lock = std::fs::canonicalize(&worktree).unwrap_or_else(|_| worktree.clone());
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
                lock,
                nested,
                argv,
                class,
                phase: Phase::Running,
                started_at: SystemTime::now(),
            },
        );
        id
    }

    /// Waits for run `id`'s turn at its worktree's lock (see the module
    /// docs). When it has to wait, `waiting` first gets the runs it waits
    /// for, called with the registry unlocked.
    ///
    /// The runs a run waits for are all admitted before it, so they only
    /// ever leave: a run that waits has waited since its admission.
    pub fn wait_for_worktree(&self, id: RunId, waiting: impl FnOnce(Vec<status::Run>)) {
        let inner = self.lock();
        let blockers = inner.blockers(id);
        if blockers.is_empty() {
            return;
        }
        let blockers = blockers.into_iter().map(|id| inner.describe(id)).collect();
        drop(inner);
        waiting(blockers);
        let mut inner = self.lock();
        while !inner.blockers(id).is_empty() {
            inner = self
                .finished
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Removes a finished run.
    pub fn finish(&self, id: RunId) {
        self.lock().runs.remove(&id);
        self.finished.notify_all();
    }

    /// The runs in flight, oldest first.
    pub fn snapshot(&self) -> Vec<status::Run> {
        let inner = self.lock();
        inner.runs.keys().map(|&id| inner.describe(id)).collect()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    /// The runs that run `id` waits for: admitted before it, in its
    /// worktree, and in conflict with it; none if it is nested.
    fn blockers(&self, id: RunId) -> Vec<RunId> {
        let Some(run) = self.runs.get(&id).filter(|run| !run.nested) else {
            return Vec::new();
        };
        let exclusive = |entry: &Entry| entry.class.worktree_lock() == Access::Exclusive;
        self.runs
            .range(..id)
            .filter(|(_, earlier)| {
                earlier.lock == run.lock && (exclusive(earlier) || exclusive(run))
            })
            .map(|(&earlier, _)| earlier)
            .collect()
    }

    /// Run `id` as `status` lists it.
    fn describe(&self, id: RunId) -> status::Run {
        let entry = &self.runs[&id];
        let waiting_for: Vec<String> = self.blockers(id).iter().map(RunId::to_string).collect();
        status::Run {
            id: id.to_string(),
            worktree: entry.worktree.to_string_lossy().into_owned(),
            argv: entry
                .argv
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            class: Some(entry.class),
            phase: entry.phase,
            started_at: clock::unix_seconds(entry.started_at),
            waiting_since: (!waiting_for.is_empty()).then(|| clock::unix_seconds(entry.started_at)),
            waiting_for,
        }
    }
}

/// Replaces `path` with `contents`, never leaving it half-written.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path)
}
