//! The runs in flight: what `status` lists, the source of run ids, and the
//! worktree lock.
//!
//! A run is admitted before anything starts; once its client knows, it is
//! starting (it waits for its turn at the worktree lock, below); it runs
//! once its process has started, and leaves when it ends. The registry
//! counts the clients following each run (`super::live`). A run that ends
//! while no client follows it stays listed as done, with how it ended,
//! since nobody was told; the oldest of those go once there are more than
//! [`DONE_KEPT`]. A done run holds no lock and counts as no run in flight.
//!
//! **Attaching.** A client asking for the same run as one that is starting
//! or running attaches to that run instead of starting another ([`Same`]).
//! One that comes while the run's own client is being told waits for that
//! (an instant).
//!
//! - A run without a key (compile, mutate, exec, a request for help) is the
//!   same as another when its [`Command`] is.
//! - A verify or test run is the same as another when its key is, which
//!   holds the tree: the same command after an edit is another run. This
//!   is single-flight: the key leaves the worktree's path out, so the run
//!   may be in any worktree. Until a run has the key it runs on, it takes
//!   such clients from its own worktree only: a run that waits at its
//!   worktree lock is keyed again after the wait ([`Registry::keyed`]),
//!   since the runs it waits for (a `fmt`) may change its tree, which
//!   changes the tree of every client in that worktree alike, but not that
//!   of another worktree. A run that waits for nothing has its key from the
//!   start.
//! - A verify or test run without a key as it asks (`--no-cache`, outside
//!   git, a key that cannot be computed) attaches to no run. One keyed
//!   once admitted (`--no-cache`) takes the clients of its key from then
//!   on.
//!
//! **Cancelling** marks a run cancelled: it takes no new clients, ends as
//! cancelled, and stops waiting for its turn. A run that runs hands out its
//! process tree (`super::tree`) to kill; one that has not started never
//! will.
//!
//! **Run ids** are `r<number>`, unique for the life of the state directory
//! ([`RunIds`]): run logs and markers are keyed by them, and on Windows
//! every logon session has a daemon of its own on the same state directory.
//!
//! **The worktree lock** (`DESIGN.md` §5). A run holds its worktree's lock
//! shared when it reads the sources and exclusive when it rewrites them, as
//! its class says (`Class::worktree_lock`). Turns follow admission order: a
//! run waits ([`Registry::wait_for_worktree`]) until no run admitted before
//! it in the same worktree conflicts with it, and two runs conflict when
//! either is exclusive. So a mutate run waits for every earlier run in its
//! worktree, every later one waits for it, and a mutate run that waits is
//! never overtaken by runs that would keep it waiting. A run holds the lock
//! from the end of its wait until it ends. The worktree is
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
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use super::key::Key;
use super::live::Live;
use super::tree::Tree;
use crate::class::{Access, Class};
use crate::protocol::{Cache, RunRequest};
use crate::status::{self, Exit, Phase};
use crate::{clock, environment, runlog};

/// How many runs that ended while no client followed them stay listed.
pub const DONE_KEPT: usize = 32;

/// How long a client asking for the same command as a run whose own client
/// is being told it started waits for that, before it starts a run of its
/// own.
const TELLING_WAIT: Duration = Duration::from_secs(10);

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

impl Command {
    /// The command `request` asks for.
    pub fn of(request: &RunRequest) -> Self {
        let mut env: Vec<(OsString, OsString)> = request
            .env
            .iter()
            .map(|(name, value)| (environment::folded(&name.0), value.0.clone()))
            .filter(|(name, _)| {
                !environment::SHELL_BOOKKEEPING
                    .iter()
                    .any(|shell| name == shell)
            })
            .collect();
        env.sort();
        Self {
            cwd: request.cwd(),
            argv: request.argv(),
            env,
        }
    }
}

/// What makes a run the same as another, for attaching (see the module
/// docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Same {
    /// A run without a key: the same [`Command`].
    Command,
    /// A verify or test run: the same key, by its digest.
    Key(String),
    /// A verify or test run without a key: no other run.
    Nothing,
}

impl Same {
    /// What `request` is the same as: its `key` (`None` when it has none)
    /// when it uses the result store, its command when it does not.
    pub fn of(request: &RunRequest, key: Option<&Key>) -> Self {
        match (request.cache, key) {
            (Cache::Off, _) => Self::Command,
            (Cache::On | Cache::Refresh, Some(key)) => Self::Key(key.digest.clone()),
            (Cache::On | Cache::Refresh, None) => Self::Nothing,
        }
    }
}

/// What the registry knows of a run.
struct Entry {
    worktree: PathBuf,
    /// The worktree as the lock tells worktrees apart: its canonical path.
    lock: PathBuf,
    /// Asked for from inside another heli run: it does not wait.
    nested: bool,
    command: Command,
    /// Which clients it takes.
    same: Same,
    /// Its key is the one it runs on: it takes the clients of that key from
    /// any worktree, and not only from its own.
    settled: bool,
    class: Class,
    /// Its phase while in flight; once it has ended, it is done.
    phase: Phase,
    started_at: SystemTime,
    /// The clients following it.
    attached: u32,
    /// `cargo heli cancel` asked for its end.
    cancelled: bool,
    state: State,
}

enum State {
    /// Admitted; its client is being told.
    Telling,
    /// Its client knows; it waits for its turn, then starts. Clients
    /// follow `live`.
    Starting { live: Arc<Live> },
    /// Its process tree runs.
    Running { live: Arc<Live>, tree: Arc<Tree> },
    /// It ended while no client followed it.
    Done { exit: Exit, ended_at: SystemTime },
}

impl State {
    fn in_flight(&self) -> bool {
        !matches!(self, Self::Done { .. })
    }
}

impl Entry {
    /// Whether it takes a client asking for `command`, which is the same as
    /// runs as `same` says, from the worktree whose lock is `lock`.
    fn takes(&self, command: &Command, same: &Same, lock: &Path) -> bool {
        if !self.state.in_flight() || self.cancelled {
            return false;
        }
        match (same, &self.same) {
            (Same::Command, Same::Command) => self.command == *command,
            (Same::Key(asked), Same::Key(key)) => {
                asked == key && (self.settled || self.lock == lock)
            }
            _ => false,
        }
    }
}

/// How a client's run request is met.
pub enum Admission {
    /// A new run, admitted: tell the client, then start it. It `waits`
    /// for runs at its worktree lock, which may change its tree before it
    /// starts.
    New { id: RunId, waits: bool },
    /// The same run is in flight: follow it.
    Attached(Attached),
    /// Neither; the reason.
    Refused(String),
}

/// A run in flight that a client has attached to.
pub struct Attached {
    pub id: RunId,
    pub live: Arc<Live>,
    /// How long it has been going.
    pub running_for: Duration,
    /// For a client attached by key: the run's worktree, which may be
    /// another than the client's.
    pub keyed_in: Option<PathBuf>,
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
    /// Signalled when a run's client has been told, a run ends or leaves,
    /// or one is cancelled: for the runs waiting for their worktree, and
    /// the clients waiting to attach.
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

    /// Attaches a client to a run in flight that is the same as `same`
    /// says, if there is one; else admits a new run of `command`, of
    /// `class`, in `worktree`, unless the daemon is `retiring`. A `nested`
    /// run was asked for from inside another heli run.
    pub fn admit(
        &self,
        worktree: PathBuf,
        command: Command,
        same: Same,
        class: Class,
        nested: bool,
        retiring: bool,
    ) -> Admission {
        let lock = std::fs::canonicalize(&worktree).unwrap_or_else(|_| worktree.clone());
        let deadline = Instant::now() + TELLING_WAIT;
        let mut inner = self.lock();
        loop {
            let found = inner
                .runs
                .iter()
                .find(|(_, entry)| entry.takes(&command, &same, &lock))
                .map(|(&id, entry)| (id, matches!(entry.state, State::Telling)));
            match found {
                Some((id, false)) => {
                    if let Some(entry) = inner.runs.get_mut(&id)
                        && let State::Starting { live } | State::Running { live, .. } = &entry.state
                    {
                        let live = Arc::clone(live);
                        entry.attached += 1;
                        return Admission::Attached(Attached {
                            id,
                            live,
                            running_for: entry.started_at.elapsed().unwrap_or_default(),
                            keyed_in: matches!(same, Same::Key(_)).then(|| entry.worktree.clone()),
                        });
                    }
                    break;
                }
                Some((_, true)) if Instant::now() < deadline => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    inner = self
                        .changed
                        .wait_timeout(inner, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                _ => break,
            }
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
                lock,
                nested,
                command,
                same,
                settled: false,
                class,
                phase: Phase::Running,
                started_at: SystemTime::now(),
                // The client that asked for it.
                attached: 1,
                cancelled: false,
                state: State::Telling,
            },
        );
        // The runs it waits for are admitted before it, so they only ever
        // leave: one that waits for none now never will, and keeps its key.
        let waits = !inner.blockers(id).is_empty();
        if let Some(entry) = inner.runs.get_mut(&id) {
            entry.settled = !waits;
        }
        Admission::New { id, waits }
    }

    /// Run `id`, admitted and done waiting, was keyed again: `key` is the
    /// key it runs on (`None`: it has none). It now takes the clients of
    /// that key from any worktree. A run without a key is left as it is.
    pub fn keyed(&self, id: RunId, key: Option<&Key>) {
        if let Some(entry) = self.lock().runs.get_mut(&id)
            && entry.same != Same::Command
        {
            entry.same = key.map_or(Same::Nothing, |key| Same::Key(key.digest.clone()));
            entry.settled = true;
        }
    }

    /// Run `id`'s client knows it started; its clients follow `live`.
    pub fn told(&self, id: RunId, live: Arc<Live>) {
        if let Some(entry) = self.lock().runs.get_mut(&id) {
            entry.state = State::Starting { live };
        }
        self.changed.notify_all();
    }

    /// Run `id` moves to `phase`.
    pub fn set_phase(&self, id: RunId, phase: Phase) {
        if let Some(entry) = self.lock().runs.get_mut(&id) {
            entry.phase = phase;
        }
    }

    /// Waits for run `id`'s turn at its worktree's lock (see the module
    /// docs), or until it is cancelled. When it has to wait, `waiting`
    /// first gets the runs it waits for, called with the registry unlocked.
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
        while !inner.blockers(id).is_empty() && !inner.cancelled(id) {
            inner = self
                .changed
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Whether `cargo heli cancel` asked for run `id`'s end.
    pub fn cancelled(&self, id: RunId) -> bool {
        self.lock().cancelled(id)
    }

    /// Run `id`'s process (of its first phase, or a later one) has started
    /// as the first of `tree`. False when the run was cancelled meanwhile:
    /// kill the tree.
    pub fn run(&self, id: RunId, tree: Arc<Tree>) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner.runs.get_mut(&id) else {
            return false;
        };
        match &mut entry.state {
            State::Starting { live } => {
                entry.state = State::Running {
                    live: Arc::clone(live),
                    tree,
                };
            }
            State::Running { tree: current, .. } => *current = tree,
            State::Telling | State::Done { .. } => {}
        }
        !entry.cancelled
    }

    /// Marks run `run` (`r<number>`) as cancelled. Returns its process tree
    /// to kill, or `None` for a run that has not started (and now never
    /// will); the error says why it cannot be cancelled.
    pub fn cancel(&self, run: &str) -> Result<Option<Arc<Tree>>, String> {
        let mut inner = self.lock();
        let entry = runlog::run_number(run).and_then(|number| inner.runs.get_mut(&RunId(number)));
        let Some(entry) = entry else {
            return Err(format!("no run {run} is in flight"));
        };
        let tree = match &entry.state {
            State::Done { exit, .. } => {
                return Err(format!("{run} has already ended: {}", exit.describe()));
            }
            State::Running { tree, .. } => Some(Arc::clone(tree)),
            State::Telling | State::Starting { .. } => None,
        };
        entry.cancelled = true;
        drop(inner);
        self.changed.notify_all();
        Ok(tree)
    }

    /// Removes run `id`, which never ran (its client did not hear of it,
    /// its key had passed, or it could not be started).
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
            cancelled: inner.cancelled(id),
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
                .filter(|(_, entry)| !entry.state.in_flight())
                .map(|(id, _)| *id)
                .collect();
            for old in &done[..done.len().saturating_sub(DONE_KEPT)] {
                inner.runs.remove(old);
            }
        }
        drop(inner);
        self.changed.notify_all();
        exit
    }

    /// Whether any run is in flight.
    pub fn busy(&self) -> bool {
        self.lock()
            .runs
            .values()
            .any(|entry| entry.state.in_flight())
    }

    /// Whether run `run` (`r<number>`) is listed: in flight, or ended
    /// unfollowed.
    pub fn lists(&self, run: &str) -> bool {
        runlog::run_number(run).is_some_and(|number| self.lock().runs.contains_key(&RunId(number)))
    }

    /// The runs in flight and those that ended unfollowed, oldest first.
    pub fn snapshot(&self) -> Vec<status::Run> {
        let inner = self.lock();
        inner.runs.keys().map(|&id| inner.describe(id)).collect()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    /// The runs that run `id` waits for: in flight, admitted before it, in
    /// its worktree, and in conflict with it; none if it is nested.
    fn blockers(&self, id: RunId) -> Vec<RunId> {
        let Some(run) = self.runs.get(&id).filter(|run| !run.nested) else {
            return Vec::new();
        };
        let exclusive = |entry: &Entry| entry.class.worktree_lock() == Access::Exclusive;
        self.runs
            .range(..id)
            .filter(|(_, earlier)| {
                earlier.state.in_flight()
                    && earlier.lock == run.lock
                    && (exclusive(earlier) || exclusive(run))
            })
            .map(|(&earlier, _)| earlier)
            .collect()
    }

    fn cancelled(&self, id: RunId) -> bool {
        self.runs.get(&id).is_some_and(|entry| entry.cancelled)
    }

    /// Run `id` as `status` lists it.
    fn describe(&self, id: RunId) -> status::Run {
        let entry = &self.runs[&id];
        let waiting_for: Vec<String> = self.blockers(id).iter().map(RunId::to_string).collect();
        let (phase, ended_at, exit) = match &entry.state {
            State::Done { exit, ended_at } => (
                Phase::Done,
                Some(clock::unix_seconds(*ended_at)),
                Some(*exit),
            ),
            _ => (entry.phase, None, None),
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
            class: Some(entry.class),
            phase,
            started_at: clock::unix_seconds(entry.started_at),
            waiting_since: (!waiting_for.is_empty()).then(|| clock::unix_seconds(entry.started_at)),
            waiting_for,
            // The broker's to tell (`crate::pool::broker`).
            slots: 0,
            share: 0.0,
            attached_clients: entry.attached,
            ended_at,
            exit,
        }
    }
}

/// Replaces `path` with `contents`, never leaving it half-written.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path)
}
