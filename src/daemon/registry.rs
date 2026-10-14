//! The runs in flight: what `status` lists, and the source of run ids.
//!
//! A run is admitted before anything starts, runs once its process has
//! started, and leaves when it ends. The registry counts the clients
//! following each run (`super::live`). A run that ends while no client
//! follows it stays listed as done, with how it ended, since nobody was
//! told; the oldest of those go once there are more than [`DONE_KEPT`].
//!
//! **Attaching.** A client asking for the same [`Command`] as a run in
//! flight attaches to that run instead of starting another; for one still
//! starting, it waits until it has started ([`STARTING_WAIT`] at most).
//!
//! **Cancelling** marks a run cancelled (it takes no new clients, and ends
//! as cancelled) and hands out its process tree (`super::tree`) to kill.
//!
//! **Run ids** are `r<number>`, unique for the life of the state directory
//! ([`RunIds`]): run logs and markers are keyed by them, and on Windows
//! every logon session has a daemon of its own on the same state directory.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use super::live::Live;
use super::tree::Tree;
use crate::protocol::RunRequest;
use crate::runlog::Events;
use crate::status::{self, Exit, Phase};
use crate::{clock, runlog};

/// How many runs that ended while no client followed them stay listed.
pub const DONE_KEPT: usize = 32;

/// A run's id, `r<number>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunId(u64);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// What makes two runs the same command: the working directory (cargo
/// picks the package from it), argv, and the environment (the target
/// directory, flags and tool settings come from it), apart from the
/// variables a shell keeps for itself ([`SHELL_BOOKKEEPING`]), which differ
/// between two shells of one agent and which no build reads.
#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    cwd: PathBuf,
    argv: Vec<OsString>,
    /// Sorted by name; on Windows, names are upper-cased, as they ignore
    /// case there.
    env: Vec<(OsString, OsString)>,
}

/// Variables a shell sets for its own bookkeeping.
const SHELL_BOOKKEEPING: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

impl Command {
    /// The command `request` asks for.
    pub fn of(request: &RunRequest) -> Self {
        let mut env: Vec<(OsString, OsString)> = request
            .env
            .iter()
            .map(|(name, value)| {
                let name = if cfg!(windows) {
                    name.0.to_ascii_uppercase()
                } else {
                    name.0.clone()
                };
                (name, value.0.clone())
            })
            .filter(|(name, _)| !SHELL_BOOKKEEPING.iter().any(|shell| name == shell))
            .collect();
        env.sort();
        Self {
            cwd: PathBuf::from(&request.cwd.0),
            argv: request.argv.iter().map(|arg| arg.0.clone()).collect(),
            env,
        }
    }
}

/// What the registry knows of a run.
struct Entry {
    worktree: PathBuf,
    command: Command,
    started_at: SystemTime,
    /// The clients following it.
    attached: u32,
    state: State,
}

/// How a client's run request is met.
pub enum Admission {
    /// A new run, admitted: start it.
    New(RunId),
    /// The same command is in flight: follow it.
    Attached(Attached),
    /// Neither; the reason.
    Refused(String),
}

/// A run in flight that a client has attached to.
pub struct Attached {
    pub id: RunId,
    pub live: Arc<Live>,
    /// Its record from the start (`Live::open`).
    pub events: Option<Events>,
    /// How long it has been going.
    pub running_for: Duration,
}

enum State {
    /// Admitted; its process has not started yet.
    Starting,
    /// Its process has started; clients follow `live`. A cancelled run
    /// takes no more clients.
    Running {
        live: Arc<Live>,
        tree: Arc<Tree>,
        cancelled: bool,
    },
    /// It ended while no client followed it.
    Done { exit: Exit, ended_at: SystemTime },
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

/// How long a client asking for the same command as a run that is starting
/// waits for it to start, before it starts a run of its own.
const STARTING_WAIT: Duration = Duration::from_secs(10);

pub struct Registry {
    ids: RunIds,
    inner: Mutex<Inner>,
    /// Notified when a run that was starting has started, or never will.
    changed: Condvar,
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
            changed: Condvar::new(),
        }
    }

    /// Attaches a client to the run of `command` in flight, if there is
    /// one (waiting for it to start, if it is starting); else admits a new
    /// run of it in `worktree`, unless the daemon is `retiring`.
    pub fn admit(&self, worktree: PathBuf, command: Command, retiring: bool) -> Admission {
        let deadline = Instant::now() + STARTING_WAIT;
        let mut inner = self.lock();
        loop {
            let same = inner
                .runs
                .iter()
                .find(|(_, entry)| {
                    entry.command == command
                        && matches!(
                            entry.state,
                            State::Starting
                                | State::Running {
                                    cancelled: false,
                                    ..
                                }
                        )
                })
                .map(|(&id, entry)| (id, matches!(entry.state, State::Starting)));
            match same {
                Some((id, false)) => {
                    if let Some(entry) = inner.runs.get_mut(&id)
                        && let State::Running { live, .. } = &entry.state
                    {
                        entry.attached += 1;
                        return Admission::Attached(Attached {
                            id,
                            live: Arc::clone(live),
                            // Opened while it runs, so it is not pruned first.
                            events: live.open(),
                            running_for: entry.started_at.elapsed().unwrap_or_default(),
                        });
                    }
                }
                // Attach once it has started; if it does not, start anew.
                Some((_, true)) if Instant::now() < deadline => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    inner = self
                        .changed
                        .wait_timeout(inner, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                    continue;
                }
                _ => {}
            }
            break;
        }
        if retiring {
            return Admission::Refused("the heli daemon is shutting down".to_owned());
        }
        let id = match self.ids.next() {
            Ok(id) => id,
            Err(error) => {
                return Admission::Refused(format!(
                    "the heli daemon cannot number the run: {error}"
                ));
            }
        };
        inner.runs.insert(
            id,
            Entry {
                worktree,
                command,
                started_at: SystemTime::now(),
                // The client that asked for it.
                attached: 1,
                state: State::Starting,
            },
        );
        Admission::New(id)
    }

    /// Run `id`'s process has started as the first of `tree`, and its
    /// output goes to `live`.
    pub fn start(&self, id: RunId, live: Arc<Live>, tree: Tree) {
        if let Some(entry) = self.lock().runs.get_mut(&id) {
            entry.state = State::Running {
                live,
                tree: Arc::new(tree),
                cancelled: false,
            };
        }
        self.changed.notify_all();
    }

    /// Marks run `run` (`r<number>`) as cancelled, and returns its process
    /// tree to kill; the error says why there is none.
    pub fn cancel(&self, run: &str) -> Result<Arc<Tree>, String> {
        let mut inner = self.lock();
        let entry = runlog::run_number(run).and_then(|number| inner.runs.get_mut(&RunId(number)));
        match entry.map(|entry| &mut entry.state) {
            None => Err(format!("no run {run} is in flight")),
            Some(State::Starting) => Err(format!("{run} is only starting; try again")),
            Some(State::Running {
                tree, cancelled, ..
            }) => {
                *cancelled = true;
                Ok(Arc::clone(tree))
            }
            Some(State::Done { exit, .. }) => {
                Err(format!("{run} has already ended: {}", exit.describe()))
            }
        }
    }

    /// Removes run `id`, which never started.
    pub fn abandon(&self, id: RunId) {
        self.lock().runs.remove(&id);
        self.changed.notify_all();
    }

    /// A client no longer follows run `id`.
    pub fn detach(&self, id: RunId) {
        if let Some(entry) = self.lock().runs.get_mut(&id) {
            entry.attached = entry.attached.saturating_sub(1);
        }
    }

    /// Run `id` has ended with `code` (on Unix, of `signal`), and all its
    /// output is recorded; returns how it ended. Clients still following it
    /// are told next, so it leaves the registry; with none, it stays listed
    /// as done.
    pub fn end(&self, id: RunId, code: i32, signal: Option<i32>) -> Exit {
        let mut inner = self.lock();
        let exit = Exit {
            code,
            signal,
            cancelled: inner.runs.get(&id).is_some_and(|entry| {
                matches!(
                    entry.state,
                    State::Running {
                        cancelled: true,
                        ..
                    }
                )
            }),
        };
        let followed = inner.runs.get(&id).is_some_and(|entry| entry.attached > 0);
        if followed {
            inner.runs.remove(&id);
        } else if let Some(entry) = inner.runs.get_mut(&id) {
            entry.state = State::Done {
                exit,
                ended_at: SystemTime::now(),
            };
            let done: Vec<RunId> = inner
                .runs
                .iter()
                .filter(|(_, entry)| matches!(entry.state, State::Done { .. }))
                .map(|(id, _)| *id)
                .collect();
            for old in &done[..done.len().saturating_sub(DONE_KEPT)] {
                inner.runs.remove(old);
            }
        }
        exit
    }

    /// The runs in flight and those that ended unfollowed, oldest first.
    pub fn snapshot(&self) -> Vec<status::Run> {
        self.lock()
            .runs
            .iter()
            .map(|(id, entry)| {
                let (phase, ended_at, exit) = match &entry.state {
                    State::Starting | State::Running { .. } => (Phase::Running, None, None),
                    State::Done { exit, ended_at } => (
                        Phase::Done,
                        Some(clock::unix_seconds(*ended_at)),
                        Some(*exit),
                    ),
                };
                status::Run {
                    id: id.to_string(),
                    worktree: entry.worktree.to_string_lossy().into_owned(),
                    argv: entry
                        .command
                        .argv
                        .iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect(),
                    phase,
                    started_at: clock::unix_seconds(entry.started_at),
                    attached_clients: entry.attached,
                    ended_at,
                    exit,
                }
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
