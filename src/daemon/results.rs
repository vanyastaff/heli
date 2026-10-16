//! The result store (DESIGN.md §7): the verify and test runs that passed,
//! by key ([`key`](super::key)), so the same run is not repeated.
//!
//! Each pass is a marker `<state>/verified/<digest>` naming the run, its
//! worktree and the time, written through a staged file renamed into place,
//! so a reader sees a whole marker or none. The store keeps at most [`CAP`]
//! markers; a hit counts as a use, and the least recently used go first.
//!
//! A run's part, in the daemon's run sequence:
//!
//! 1. [`Store::before`]: before the run waits for anything, its key is
//!    computed and looked up; a hit ends it. On a miss the key goes on to
//!    single-flight (`super::registry`), so that matching runs in flight
//!    costs no snapshot of its own.
//! 2. [`Store::admitted`]: once admitted, the key is looked up again
//!    (another run may have recorded it meanwhile). After a wait at the
//!    worktree lock it is computed again first, since the runs waited for
//!    may have changed the tree; a run that waited for nothing keeps the
//!    key of step 1, taken a moment before. This snapshot of the tree is
//!    the one taken before the run's steps.
//! 3. [`Store::after`]: a run that passed is recorded when the tree, taken
//!    again after its steps, is the snapshot of step 2.
//!
//! `--no-cache` skips the lookups, not the recording. Every part is best
//! effort: a key that cannot be computed, or a store that cannot be written,
//! costs the saving, never the run.
//! `--cache off` computes a key for single-flight only: it never reads,
//! records or invalidates persistent verdicts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::clock;
use crate::daemon::key::{self, Key};
use crate::protocol::{Cache, Reply, RunRequest, Text};
use crate::run_id::RunId;
use crate::worktree;

/// The most markers kept.
pub const CAP: usize = 256;

/// A pass, as its marker holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub run: RunId,
    pub worktree: Text,
    /// Seconds since 1970.
    pub at: u64,
}

impl Record {
    /// The reply that ends a run whose key this record holds.
    pub fn reply(self) -> Reply {
        Reply::Passed {
            run: self.run,
            worktree: self.worktree,
            at: self.at,
        }
    }
}

/// What the result store finds before a run waits for anything.
pub enum Before {
    /// The key already passed: nothing runs.
    Passed(Record),
    /// No pass is recorded: the run goes on, with its key when it has one
    /// (it looks the store up, in a git worktree).
    Miss(Option<Key>),
}

/// What the result store decides once a run is admitted.
pub enum Admitted {
    /// The key already passed: nothing runs.
    Passed(Record),
    /// The run goes ahead; with a key, it is recorded if it passes. The note
    /// says why a run that uses the store has no key.
    Run {
        key: Option<Key>,
        note: Option<String>,
    },
}

/// The store in a state directory.
pub struct Store {
    /// The markers.
    dir: PathBuf,
    /// Scratch files of key computations.
    scratch: PathBuf,
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("verified"),
            scratch: state_dir.join("scratch"),
        }
    }

    /// Before `request` waits for anything: its key, and the pass of that
    /// key if persistent reuse is enabled. Live-only and forced-new runs
    /// are keyed too, to protect their source generation during admission.
    pub fn before(&self, request: &RunRequest) -> Before {
        if !matches!(request.cache, Cache::On | Cache::Live | Cache::Refresh) {
            return Before::Miss(None);
        }
        let Some(key) = key::compute(request, &self.scratch).ok().flatten() else {
            return Before::Miss(None);
        };
        if request.cache == Cache::On
            && let Some(record) = self.lookup(&key)
        {
            return Before::Passed(record);
        }
        Before::Miss(Some(key))
    }

    /// Once `request` is admitted: its key, and its pass if one is recorded
    /// now. The key is `known` when given (the key of [`Store::before`], for
    /// a run that waited for nothing), else computed again.
    pub fn admitted(&self, request: &RunRequest, known: Option<Key>) -> Admitted {
        if matches!(request.cache, Cache::Off | Cache::Disabled) {
            return Admitted::Run {
                key: None,
                note: None,
            };
        }
        let key = match known {
            Some(known) => Some(known),
            None => match key::compute(request, &self.scratch) {
                Ok(key) => key,
                Err(error) => {
                    return Admitted::Run {
                        key: None,
                        note: Some(format!(
                            "cannot key this run, so it is not shared or recorded: {error}"
                        )),
                    };
                }
            },
        };
        if request.cache == Cache::On
            && let Some(record) = key.as_ref().and_then(|key| self.lookup(key))
        {
            return Admitted::Passed(record);
        }
        Admitted::Run { key, note: None }
    }

    /// After run `run` of `request`, keyed `key`, ended: records it when it
    /// `passed` with the inputs it started with, its key computed again
    /// (the tree, and the configuration and toolchain it read, may have
    /// changed meanwhile). A run that failed drops the pass recorded for
    /// its key, which a flaky run may have left. Returns a note when it
    /// passed but is not recorded.
    pub fn after(
        &self,
        request: &RunRequest,
        key: &Key,
        run: RunId,
        passed: bool,
    ) -> Option<String> {
        // A Live key identifies running work only. Neither success nor
        // failure may modify a verdict independently saved by another run.
        if request.cache == Cache::Live {
            return None;
        }
        if !passed {
            let _ = std::fs::remove_file(self.marker(key));
            return None;
        }
        match key::compute(request, &self.scratch) {
            Ok(Some(now)) if now.snapshot != key.snapshot => {
                return Some(
                    "the worktree changed during the run, so it is not recorded".to_owned(),
                );
            }
            Ok(Some(now)) if now.digest == key.digest => {}
            Ok(_) => {
                return Some(
                    "the run's inputs changed during the run, so it is not recorded".to_owned(),
                );
            }
            Err(error) => {
                return Some(format!(
                    "cannot key the run again, so it is not recorded: {error}"
                ));
            }
        }
        let record = Record {
            run,
            worktree: Text(worktree::of(&request.cwd()).into_os_string()),
            at: clock::now(),
        };
        self.record(key, &record)
            .err()
            .map(|error| format!("cannot record the run: {error}"))
    }

    fn marker(&self, key: &Key) -> PathBuf {
        self.dir.join(&key.digest)
    }

    /// The pass recorded for `key`, which counts as a use of its marker.
    fn lookup(&self, key: &Key) -> Option<Record> {
        let marker = self.marker(key);
        let record = serde_json::from_slice(&std::fs::read(&marker).ok()?).ok()?;
        if let Ok(file) = std::fs::File::options().write(true).open(&marker) {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(record)
    }

    /// Writes `record` as the marker of `key`, then drops the least recently
    /// used markers beyond [`CAP`].
    fn record(&self, key: &Key, record: &Record) -> std::io::Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(&self.dir)?;
        let marker = self.marker(key);
        let staged = self.dir.join(format!(
            "{}.{}-{}.tmp",
            key.digest,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let text = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        std::fs::write(&staged, text)
            .and_then(|()| std::fs::rename(&staged, &marker))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&staged);
            })?;
        self.prune(&marker);
        Ok(())
    }

    /// Removes the least recently used markers beyond [`CAP`], never
    /// `written`.
    fn prune(&self, written: &Path) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut markers: Vec<(SystemTime, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|entry| is_marker(&entry.file_name()) && entry.path() != written)
            .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
            .collect();
        let excess = (markers.len() + 1).saturating_sub(CAP);
        markers.sort();
        for (_, path) in markers.iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Whether `name` is a marker's: a digest, 64 hex digits.
fn is_marker(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
}
