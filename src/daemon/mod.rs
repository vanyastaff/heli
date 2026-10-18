//! `cargo heli daemon`: the per-user daemon that executes runs for clients.
//!
//! A client starts it on demand (`crate::launch`); it is never a service.
//! At start it takes the lock in the state directory (one daemon per user,
//! and on Windows per logon session) and listens on the owner-only endpoint
//! (`crate::ipc`). Each connection gets a thread, which speaks
//! `crate::protocol`:
//!
//! - **run**: look its key up in the result store ([`results`], keyed by
//!   [`key`]), then attach the client to the same run in flight, if any:
//!   the same command, or for a verify or test run the same key, from any
//!   worktree (single-flight); else admit it to the registry
//!   ([`registry`]) and the event log
//!   ([`events`]), tell the client it started, and then, in a thread of the
//!   run's own: wait for its turn at the worktree lock, look its key up
//!   again, start its record in the run log (`crate::runlog`), give it a
//!   run pool (`crate::pool::broker`), execute it, each phase in a process
//!   tree of its own ([`execute`], [`tree`]; a test run in two phases,
//!   [`test_run`]), record it in the result store if it passed, return its
//!   run pool's loans, take it out of the registry. Its clients follow it
//!   meanwhile ([`live`]): its output from the record, then its exit.
//!   `Daemon::start` and `Daemon::execute` are the one sequence every run
//!   goes through, and the place for what happens around a run (admission,
//!   waits, recording);
//! - **status**: describe the daemon and the registry's runs;
//! - **log**: send part of a run's record;
//! - **cancel**: kill a run's process tree;
//! - **stop**: say how many runs are in flight, then retire, as below;
//! - a request it does not know: answer that it is unsupported;
//! - **retire**, from a newer client (of another protocol version, or with a
//!   request this daemon does not know): accept no more runs and exit once
//!   the runs in flight have reported their exits.
//!
//! What the daemon owns for its whole life (the lock, then the host pool,
//! `crate::pool`, and its broker, its memory [`governor`], the event log
//! and the run log,
//! `crate::runlog`, all as configured, and on Unix the pool's [`warden`])
//! is set up in [`main`]. The daemon
//! runs until it is killed or retired, or in a debug build until its
//! [`owner`] is gone. Its own log (start, end, errors)
//! is `daemon.log` in the state directory (`daemon-<session>.log` on
//! Windows), since it has no terminal.

pub mod accounts;
pub mod advisory;
pub mod events;
pub mod execute;
pub mod governor;
pub mod key;
pub mod live;
pub mod owner;
pub mod registry;
pub mod results;
pub mod test_run;
pub mod tree;
pub mod warden;

use std::ffi::{OsStr, OsString};
use std::fs::{File, TryLockError};
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use crate::class::Class;
use crate::config::Config;
use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::pool::broker::{Broker, Loans};
use crate::pool::{self, Pool};
use crate::protocol::{self, CancelRequest, Channel, LogRequest, Reply, Request, RunRequest, Text};
use crate::runlog::{self, Limits, Recorder, RunLog};
use crate::status::{self, Phase, Status};
use crate::{clock, nesting, session, worktree};

use crate::run_id::RunId;
use events::Events;
use governor::Governor;
use key::Key;
use live::Live;
use registry::{Admission, Attached, Command, Registry, RunIds, Same};
use results::{Admitted, Before};
use test_run::TestRun;

/// The daemon's exit code when another daemon already holds the lock: the
/// client that started it then connects to that one.
pub const ALREADY_RUNNING: i32 = 3;

/// How long a daemon tries to create its pipe while a predecessor's is
/// still there ([`bind`]): as long as a client waits for a start.
const BIND_WAIT: Duration = Duration::from_secs(5);

/// How long a retired daemon tries to wake its own main thread, to exit
/// there ([`Daemon::exit_if_retired`]), before it exits where it is.
const WAKE_WAIT: Duration = Duration::from_secs(1);

/// The longest a test run waits for its test slots (DESIGN.md §5, "Where a
/// run can wait").
const TEST_SLOT_WAIT: Duration = Duration::from_secs(5);

/// Runs the daemon until it is killed or retires.
pub fn main() -> ! {
    let locations = match Locations::resolve() {
        Ok(locations) => locations,
        Err(error) => fail(None, &error),
    };
    if let Err(error) = locations.create_state_dir() {
        fail(
            None,
            &format!("cannot create {}: {error}", locations.state_dir.display()),
        );
    }
    let log = Log::open(&locations);
    let lock = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locations.lock_file())
    {
        Ok(lock) => lock,
        Err(error) => fail(Some(&log), &format!("cannot open the lock: {error}")),
    };
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            log.write("another daemon holds the lock; exiting");
            std::process::exit(ALREADY_RUNNING)
        }
        Err(TryLockError::Error(error)) => {
            fail(Some(&log), &format!("cannot take the lock: {error}"))
        }
    }
    let config = Config::load(&locations.config).unwrap_or_else(|error| fail(Some(&log), &error));
    advisory::set_interval(std::time::Duration::from_secs(
        config.advisories.fetch_interval_minutes.saturating_mul(60),
    ));
    let pool = Arc::new(open_pool(&locations, &config, &log));
    let governor = Governor::new(&config, locations.config.clone())
        .unwrap_or_else(|error| fail(Some(&log), &error));
    let events = Arc::new(Events::configured(&locations, &config));
    #[cfg(unix)]
    let _warden = start_warden(&locations, &log);

    let daemon = Arc::new(Daemon {
        accounts: accounts::Accounts::start(
            config.store.clone(),
            locations.state_dir.clone(),
            locations.config.clone(),
            log.clone(),
            Arc::clone(&events),
        ),
        registry: Registry::new(RunIds::new(
            locations.run_counter(),
            locations.run_counter_lock(),
            locations.run_logs(),
        )),
        broker: Broker::start(Arc::clone(&pool)),
        pool,
        governor,
        events,
        runlog: RunLog::new(locations.run_logs(), Limits::configured(&config.log)),
        results: results::Store::new(&locations.state_dir),
        lifecycle: Mutex::new(Lifecycle::default()),
        started_at: SystemTime::now(),
        log,
        endpoint: locations.endpoint.clone(),
        exiting: AtomicBool::new(false),
        waker: Mutex::new(None),
        lock,
    });
    {
        let daemon = Arc::clone(&daemon);
        let broker = Arc::clone(&daemon.broker);
        let lent = move || broker.lent();
        std::thread::spawn(move || {
            daemon.governor.run(
                &daemon.pool,
                &daemon.registry,
                &daemon.events,
                &daemon.log,
                lent,
            )
        });
    }
    if let Some(owner) = owner::pid() {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || {
            owner::wait_for_exit(owner);
            daemon.orphaned(owner)
        });
    }
    // Listening last: a client that finds the endpoint finds the daemon
    // ready to accept it.
    let mut listener = bind(&locations, &daemon.log);
    daemon.log.write(&format!(
        "started, pid {}, protocol {}, endpoint {}",
        std::process::id(),
        protocol::VERSION,
        locations.endpoint.display()
    ));
    let open = Arc::new(AtomicUsize::new(0));
    loop {
        let accepted = listener.accept();
        if daemon.exiting.load(Ordering::SeqCst) {
            daemon.exit(listener);
        }
        match accepted {
            Ok(stream) => match Connection::claim(&open) {
                Some(connection) => {
                    let daemon = Arc::clone(&daemon);
                    std::thread::spawn(move || {
                        let _connection = connection;
                        daemon.serve(stream);
                    });
                }
                // Closed unanswered: its client runs the command directly,
                // as with no daemon at all.
                None => daemon.log.write(&format!(
                    "refused a connection: {MAX_CONNECTIONS} are open already"
                )),
            },
            Err(error) => {
                daemon.log.write(&format!("accept failed: {error}"));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// The most connections the daemon serves at once, each on a thread of its
/// own: every client in flight holds one (a run's, attached ones, status,
/// log), so this bounds the daemon's threads without limiting a host's
/// agents in practice.
const MAX_CONNECTIONS: usize = 512;

/// A connection's place among [`MAX_CONNECTIONS`]; dropping it frees the
/// place, however the connection's thread ends.
struct Connection(Arc<AtomicUsize>);

impl Connection {
    /// A place for one more connection, unless all are taken.
    fn claim(open: &Arc<AtomicUsize>) -> Option<Self> {
        // Taken first, given back if it was one too many: the count never
        // stays above the bound.
        let connection = Self(Arc::clone(open));
        (open.fetch_add(1, Ordering::SeqCst) < MAX_CONNECTIONS).then_some(connection)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Listens at the endpoint, or exits with failure. This daemon holds the
/// lock, so a pipe already there on Windows is a predecessor's that has
/// released the lock but not yet closed its last connections as its process
/// goes (or someone else's): it tries again for [`BIND_WAIT`].
fn bind(locations: &Locations, log: &Log) -> ipc::Listener {
    let deadline = Instant::now() + BIND_WAIT;
    loop {
        match ipc::Listener::bind(&locations.endpoint) {
            Ok(listener) => return listener,
            Err(error)
                if cfg!(windows)
                    && error.kind() == std::io::ErrorKind::PermissionDenied
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => fail(
                Some(log),
                &format!("cannot listen at {}: {error}", locations.endpoint.display()),
            ),
        }
    }
}

/// Logs `error` where it can, and exits with failure.
fn fail(log: Option<&Log>, error: &str) -> ! {
    match log {
        Some(log) => log.write(error),
        None => eprintln!("cargo-heli daemon: {error}"),
    }
    std::process::exit(1)
}

/// Opens the host pool, of the size configured, or exits with failure.
fn open_pool(locations: &Locations, config: &Config, log: &Log) -> Pool {
    let slots = config
        .pool
        .slots
        .map_or_else(pool::default_slots, NonZeroU32::get);
    let name = locations.pool.display();
    match Pool::open(&locations.pool, slots) {
        Ok((pool, opened)) => {
            let status = pool.status();
            log.write(&format!(
                "pool {name}: {}, {} slots, {} free",
                match opened {
                    pool::Opened::Created => "created",
                    pool::Opened::Adopted => "adopted",
                },
                status.slots,
                status.free
            ));
            pool
        }
        Err(error) => fail(Some(log), &format!("cannot open the pool {name}: {error}")),
    }
}

/// Starts the pool's warden ([`warden`]); what it returns must live as
/// long as the daemon. A daemon without a warden still serves.
#[cfg(unix)]
fn start_warden(locations: &Locations, log: &Log) -> Option<std::io::PipeWriter> {
    match warden::start(locations) {
        Ok((alive, pid)) => {
            // Not a `pool ` line: each of those says how a daemon found its
            // pool.
            log.write(&format!("warden: pid {pid}"));
            Some(alive)
        }
        Err(error) => {
            log.write(&format!("cannot start the pool's warden: {error}"));
            None
        }
    }
}

struct Daemon {
    accounts: Arc<accounts::Accounts>,
    registry: Registry,
    pool: Arc<Pool>,
    /// Lends the host pool's slots to the runs' own pools.
    broker: Arc<Broker>,
    governor: Governor,
    events: Arc<Events>,
    runlog: RunLog,
    results: results::Store,
    lifecycle: Mutex<Lifecycle>,
    started_at: SystemTime,
    log: Log,
    endpoint: PathBuf,
    /// Retired with no run left: the main thread exits ([`Daemon::exit`]).
    exiting: AtomicBool,
    /// The connection that wakes the main thread to exit, kept open until it
    /// does.
    waker: Mutex<Option<Stream>>,
    /// Held until the daemon exits: a retired daemon releases it as it
    /// exits ([`Daemon::exit`]), and the operating system when the process
    /// is gone, even killed.
    lock: File,
}

/// Whether the daemon still takes runs, and how many it is serving.
#[derive(Default)]
struct Lifecycle {
    retiring: bool,
    /// Runs admitted whose clients have not yet been told the exit.
    serving: usize,
}

impl Daemon {
    /// Serves one connection.
    fn serve(&self, mut stream: Stream) {
        let Ok(version) = protocol::read_hello(&mut stream) else {
            return;
        };
        if protocol::write_hello(&mut stream).is_err() {
            return;
        }
        if version != protocol::VERSION {
            self.retire_if_asked(&mut stream, &format!("a client of protocol {version}"));
            return;
        }
        // JSON first, so that a request this daemon does not know (from a
        // newer client) is told apart from a garbled one.
        let Ok(message) = protocol::read_message::<serde_json::Value>(&mut stream) else {
            return;
        };
        match serde_json::from_value::<Request>(message) {
            Ok(Request::Status) => {
                let _ =
                    protocol::write_message(&mut stream, &Reply::Status(Box::new(self.status())));
            }
            Ok(Request::Run(request)) => self.run(stream, request),
            Ok(Request::Log(request)) => self.log(stream, &request),
            Ok(Request::Cancel(request)) => self.cancel(stream, &request),
            Ok(Request::Stop) => self.stop(stream),
            Err(_) => {
                if protocol::write_message(&mut stream, &Reply::Unsupported).is_ok() {
                    self.retire_if_asked(&mut stream, "a client with a request this daemon lacks");
                }
            }
        }
    }

    /// Retires when the client's next byte asks for it; `client` says who
    /// it is, for the log.
    fn retire_if_asked(&self, stream: &mut Stream, client: &str) {
        let mut byte = [0u8];
        if matches!(std::io::Read::read(stream, &mut byte), Ok(1)) && byte[0] == protocol::RETIRE {
            self.log
                .write(&format!("{client} asked this daemon to retire"));
            self.retire();
        }
    }

    /// Answers with the runs in flight, then retires: the answer first,
    /// since a daemon with no run exits at once.
    fn stop(&self, mut stream: Stream) {
        let runs = self
            .registry
            .snapshot()
            .iter()
            .filter(|(_, run)| run.exit.is_none())
            .count();
        let runs = u32::try_from(runs).unwrap_or(u32::MAX);
        let _ = protocol::write_message(&mut stream, &Reply::Stopping { runs });
        drop(stream);
        self.log.write(&format!(
            "a client asked this daemon to stop, with {runs} runs in flight"
        ));
        self.retire();
    }

    /// Sends the part of a run's record that `request` asks for.
    fn log(&self, mut stream: Stream, request: &LogRequest) {
        let mut out = |channel, bytes: &[u8]| protocol::write_output(&mut stream, channel, bytes);
        let read = match request.run.parse::<RunId>() {
            Err(_) => Err(runlog::ReadError::NotARun(request.run.clone())),
            Ok(run) => {
                let read = runlog::read(self.runlog.dir(), run, &request.select, &mut out);
                // A run is listed from its admission, but its record begins
                // only as its command starts, after its turn at the worktree
                // lock: until then, it has printed nothing.
                if matches!(read, Err(runlog::ReadError::Unknown(_))) && self.registry.lists(run) {
                    runlog::read_unstarted(run, &request.select, &mut out)
                } else {
                    read
                }
            }
        };
        let reply = match read {
            Ok(()) => Reply::LogEnd,
            Err(error) => Reply::LogFailed {
                message: error.to_string(),
            },
        };
        let _ = protocol::write_message(&mut stream, &reply);
    }

    /// Cancels the run `request` names: kills its process tree, and answers
    /// once none of its processes is left; a run that has not started never
    /// will. The run then ends as usual, and its clients hear that it was
    /// cancelled.
    fn cancel(&self, mut stream: Stream, request: &CancelRequest) {
        let run = &request.run;
        let cancelled = run
            .parse::<RunId>()
            .map_err(|_| format!("no run {run} is in flight"))
            .and_then(|run| self.registry.cancel(run));
        let reply = match cancelled {
            Err(message) => Reply::CancelFailed { message },
            Ok(tree) => match tree.map_or(Ok(()), |tree| tree.kill()) {
                Ok(()) => {
                    self.log.write(&format!("cancelled {run}"));
                    Reply::Cancelled
                }
                Err(error) => {
                    let message = format!("cannot cancel {run}: {error}");
                    self.log.write(&message);
                    Reply::CancelFailed { message }
                }
            },
        };
        let _ = protocol::write_message(&mut stream, &reply);
    }

    /// Executes `request` for the client on `stream`, or attaches the
    /// client to the same run in flight (single-flight, for a verify or
    /// test run: the same key, from any worktree).
    fn run(&self, mut stream: Stream, request: RunRequest) {
        let (serving, retiring) = self.serving();
        if (request.dependency_epoch.is_some()
            || matches!(
                request.cache,
                crate::protocol::Cache::On
                    | crate::protocol::Cache::Live
                    | crate::protocol::Cache::Refresh
            ))
            && request.reads_spawned
            && protocol::write_message(&mut stream, &Reply::Preparing).is_err()
        {
            return;
        }
        // Own the immutable inputs before result lookup or metadata probes.
        // Keep ownership through queueing, learning and the final key reread.
        let epoch = match request.dependency_epoch.as_ref() {
            Some(path) => match crate::dependencies::Epoch::open(Path::new(&path.0)) {
                Ok(epoch) => Some(epoch),
                Err(message) => {
                    let _ = protocol::write_message(
                        &mut stream,
                        &Reply::SpawnFailed {
                            not_found: false,
                            message: format!("cannot use dependency epoch: {message}"),
                        },
                    );
                    return;
                }
            },
            None => None,
        };
        // Before the run waits for anything: its key, which a pass may hold
        // already, and which finds the same run in flight.
        let key = match self.results.before(&request) {
            Before::Passed(passed) => {
                let _ = protocol::write_message(&mut stream, &passed.reply());
                return;
            }
            Before::Miss(key) => key,
        };
        let worktree = worktree::of(&request.cwd());
        let admission = self.registry.admit(
            worktree.clone(),
            Command::of(&request),
            Same::of(&request, key.as_ref()),
            registry::Kind {
                class: request.class.unwrap_or(Class::Exec),
                profile: request.profile.clone(),
                rerun_of: request.rerun_of.clone(),
            },
            nested(&request),
            retiring,
        );
        match admission {
            // A run that waits for nothing keeps its key: its tree is the
            // one just snapshotted.
            Admission::New { id, waits } => {
                let key = key.filter(|_| !waits);
                self.start(
                    stream,
                    Inputs {
                        request: &request,
                        epoch: epoch.as_ref(),
                    },
                    id,
                    &worktree,
                    key,
                );
            }
            Admission::Attached(run) => self.attach(stream, &request, run),
            Admission::Refused(reason) => {
                if !retiring {
                    self.log.write(&reason);
                }
                let _ = protocol::write_message(&mut stream, &Reply::Refused { reason });
            }
        }
        // The client heard how its run ended (or left) before a retiring
        // daemon may exit.
        drop(serving);
    }

    /// Follows run `run.id`, already in flight, for the client on `stream`,
    /// which asked for the same run (`request`).
    fn attach(&self, mut stream: Stream, request: &RunRequest, run: Attached) {
        let _pin = self.runlog.pin(run.id);
        let started = Reply::Started {
            run: run.id,
            attached: Some(u64::try_from(run.running_for.as_millis()).unwrap_or(u64::MAX)),
            keyed_in: run.keyed_in.map(|worktree| Text(worktree.into_os_string())),
            spawned_follows: request.reads_spawned,
        };
        if protocol::write_message(&mut stream, &started).is_err() {
            self.registry.detach(run.id);
            return;
        }
        self.follow(run.id, &run.live, stream, reads(request));
    }

    /// Starts run `id` of `request` in `worktree`, and follows it for the
    /// client on `stream`; returns once the run has ended. `known` is the
    /// key it keeps, if it keeps the one it was admitted with.
    fn start(
        &self,
        mut stream: Stream,
        inputs: Inputs<'_>,
        id: RunId,
        worktree: &Path,
        known: Option<Key>,
    ) {
        let request = inputs.request;
        let argv = request.argv();
        let admission = serde_json::json!({
            "worktree": worktree.to_string_lossy(),
            "argv": argv.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>(),
            "class": request.class.map(Class::name),
            "profile": request.profile,
        });
        // Nothing starts unless the client knows it started: a client that
        // never hears of it runs the command itself. One that reads it also
        // hears when the command has started (`Spawned`), after the waits
        // below; until then, it too may run the command itself.
        let started = Reply::Started {
            run: id,
            attached: None,
            keyed_in: None,
            spawned_follows: request.reads_spawned,
        };
        let told = protocol::write_message(&mut stream, &started);
        // Logged once the client is told: the event log's first write
        // creates it, which can take a while.
        self.events.admit(id, admission);
        if told.is_err() {
            self.registry.abandon(id);
            return;
        }
        // Kept until the run has ended (the scope below waits for it), when
        // its directories are counted again.
        let learning = self
            .accounts
            .learn(request, inputs.epoch.map(|epoch| epoch.lease_root()));
        let _pin = self.runlog.pin(id);
        let live = Arc::new(Live::new(self.runlog.dir().to_path_buf(), id));
        self.registry.told(id, Arc::clone(&live));
        // The run goes on whether or not its client follows it to the end.
        std::thread::scope(|scope| {
            scope.spawn(|| self.execute(id, inputs, &live, known, learning.as_ref()));
            self.follow(id, &live, stream, reads(request));
        });
    }

    /// Run `id` of `request`, from its turn at the worktree lock to its end;
    /// all of it reaches its clients through `live`. `known` is the key it
    /// keeps, if any (it waits for nothing). Before it starts, its build
    /// directory is recycled if `learning` says it may be.
    fn execute(
        &self,
        id: RunId,
        inputs: Inputs<'_>,
        live: &Live,
        known: Option<Key>,
        learning: Option<&accounts::Learning>,
    ) {
        let Inputs { request, epoch } = inputs;
        // Shared by all commands, including the gap between build and test
        // phases. A separate runner owns the same guard while each actual
        // child runs, so a daemon crash cannot expose its runtime assets.
        let activity =
            crate::store::lease::directory().and_then(|root| crate::store::lease::shared(&root));
        let _activity = match activity {
            Ok(activity) => RuntimeActivity {
                guard: Some(activity),
                accounts: &self.accounts,
            },
            Err(error) => {
                self.registry.abandon(id);
                live.end(Reply::SpawnFailed {
                    not_found: false,
                    message: format!("cannot protect command runtime from GC: {error}"),
                });
                return;
            }
        };
        // The worktree lock: its clients hear which runs it waits for, if
        // any.
        self.registry
            .wait_for_worktree(id, |runs| live.waiting(runs));
        if self.registry.cancelled(id) {
            let exit = self.registry.end(id, NEVER_STARTED, None);
            live.end(Reply::exited(exit));
            return;
        }
        // Admitted, and done waiting: another run may have passed with the
        // same key meanwhile, so the key is looked up again; after a wait,
        // the worktree may have changed too, so it is computed again first,
        // and the run takes the clients of the key it now has. Its snapshot
        // of the tree is the one the run starts on.
        let keep = known.is_some();
        let admitted = self.results.admitted(request, known);
        if (!keep || request.cache == crate::protocol::Cache::Refresh)
            && let Admitted::Run { key, .. } = &admitted
        {
            self.registry.keyed(id, key.as_ref(), request.cache);
        }
        let key = match admitted {
            Admitted::Passed(passed) => {
                self.registry.abandon(id);
                live.end(passed.reply());
                return;
            }
            Admitted::Run { key, note } => {
                if let Some(note) = note {
                    live.note(&note);
                }
                key
            }
        };
        // A worktree without a build directory may get a recycled one,
        // which must be in place before cargo starts (DESIGN.md §6.4).
        if let Some(recycled) = learning.and_then(accounts::Learning::prepared) {
            live.note(&recycled.describe());
        }
        let mut record = self.runlog.record(id);
        if let Some(args) = &request.invocation {
            record.start(&runlog::Start {
                args: args.clone(),
                cwd: request.cwd.clone(),
                class: request.class.unwrap_or(Class::Exec),
                rerun_of: request.rerun_of.clone(),
            });
        }
        let (makeflags, error) = self.broker.open(id);
        if let Some(error) = error {
            self.log.write(&format!(
                "{id}: no run pool ({error}); it draws from the host pool"
            ));
        }
        let mut run = Execution {
            request,
            epoch,
            id,
            makeflags: &makeflags,
            live,
            record: &mut record,
        };
        let ended = match test_run::plan(request) {
            None => self.phase(&mut run, &request.argv()),
            Some(plan) => self.test(&mut run, &plan),
        };
        if let Some(loans) = self.broker.close(id) {
            self.events.record("loans", loans_event(id, &loans));
        }
        // Its build directory's source-state record (DESIGN.md §6.4), before
        // its clients hear it ended: a worktree removed right after its last
        // run leaves a record of it.
        if let Some(learning) = learning {
            learning.ended();
        }
        if let Some(error) = record.take_error() {
            self.log.write(&format!("cannot record {id}: {error}"));
        }
        let (code, signal) = match ended {
            Ok(exited) => exited,
            Err(failed) => {
                drop(record);
                self.registry.abandon(id);
                live.end(failed);
                return;
            }
        };
        if let Some(key) = &key {
            let passed = code == 0 && signal.is_none();
            if let Some(note) = self.results.after(request, key, id, passed) {
                live.note(&note);
            }
        }
        let exit = self.registry.end(id, code, signal);
        drop(record);
        drop(_activity);
        live.end(Reply::exited(exit));
    }

    /// Runs `argv` as a phase of `run`, in a process tree of its own:
    /// returns how it exited (its code, and on Unix the signal that killed
    /// it), or the reply that tells it could not start.
    fn phase(
        &self,
        run: &mut Execution<'_, '_>,
        argv: &[OsString],
    ) -> Result<(i32, Option<i32>), Reply> {
        let activity = crate::store::lease::directory().map_err(|error| Reply::SpawnFailed {
            not_found: false,
            message: error.to_string(),
        })?;
        let mut roots = vec![activity];
        if let Some(epoch) = run.epoch {
            roots.push(epoch.lease_root().to_path_buf());
        }
        let (child, tree) = execute::spawn(run.request, argv, run.id, run.makeflags, &roots)
            .map_err(|error| Reply::SpawnFailed {
                not_found: error.kind() == std::io::ErrorKind::NotFound,
                message: error.to_string(),
            })?;
        let tree = Arc::new(tree);
        // From here on its clients never run the command themselves.
        run.live.spawned();
        if !self.registry.run(run.id, Arc::clone(&tree))
            && let Err(error) = tree.kill()
        {
            // Cancelled as it started.
            self.log
                .write(&format!("cannot cancel {}: {error}", run.id));
        }
        Ok(match execute::pump(child, run.record, run.live) {
            Ok(status) => exited(status),
            Err(error) => {
                self.log
                    .write(&format!("cannot wait for {}: {error}", run.id));
                (1, None)
            }
        })
    }

    /// Runs test run `run` in its two phases (`test_run`): the build (a
    /// request of its own), then, if it passed, the tests on as many
    /// threads as it got test slots. A header line on its stderr says how
    /// many, its share, and how long it waited for them.
    fn test(
        &self,
        run: &mut Execution<'_, '_>,
        plan: &TestRun,
    ) -> Result<(i32, Option<i32>), Reply> {
        if let Some(build) = plan.build(run.request) {
            self.registry.set_phase(run.id, Phase::Building);
            let mut phase = Execution {
                request: &build,
                epoch: run.epoch,
                id: run.id,
                makeflags: run.makeflags,
                live: run.live,
                record: &mut *run.record,
            };
            let built = self.phase(&mut phase, &build.argv())?;
            if built.0 != 0 {
                return Ok(built);
            }
        }
        self.registry.set_phase(run.id, Phase::Testing);
        let waiting = Instant::now();
        let share = self.broker.share(run.id);
        let wanted = test_slots_wanted(plan.asked(), share);
        let slots = self.broker.take_test_slots(run.id, wanted, TEST_SLOT_WAIT);
        let header = test_header(run.id, slots, share, waiting.elapsed());
        let recorded = run.record.output(Channel::Stderr, header.as_bytes());
        run.live
            .append(Channel::Stderr, header.into_bytes(), recorded);
        let test = plan.test(run.request, slots.max(1));
        let mut phase = Execution {
            request: &test,
            epoch: run.epoch,
            id: run.id,
            makeflags: run.makeflags,
            live: run.live,
            record: &mut *run.record,
        };
        let tested = self.phase(&mut phase, &test.argv());
        self.broker.release_test_slots(run.id);
        tested
    }

    /// Sends run `id`'s output to the client on `stream`, from its first
    /// byte, until the run ends or the client goes away; the client also
    /// hears what it `reads` of the run's waits and start.
    fn follow(&self, id: RunId, live: &Live, mut stream: Stream, reads: live::Reads) {
        if let Err(error) = live::follow(live, &mut stream, reads, &|| self.lifecycle().retiring) {
            self.log.write(&format!("stopped following {id}: {error}"));
        }
        drop(stream);
        self.registry.detach(id);
    }

    fn status(&self) -> Status {
        let place = session::here();
        Status {
            schema: status::SCHEMA,
            daemon: Some(status::Daemon {
                pid: std::process::id(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                executable: std::env::current_exe()
                    .ok()
                    .map(|path| path.display().to_string()),
                protocol: protocol::VERSION,
                started_at: clock::unix_seconds(self.started_at),
                interactive: place.interactive(),
                session: place.session,
                window_station: place.window_station,
                desktop: place.desktop,
            }),
            pool: Some(self.pool.status()),
            memory: self.governor.memory(),
            runs: self.runs(),
            disk: Some(self.accounts.disk()),
            last_gc: self.accounts.last_gc(),
        }
    }

    /// The runs in flight (and those that ended unfollowed), with the slots
    /// each uses.
    fn runs(&self) -> Vec<status::Run> {
        let usage = self.broker.usage();
        self.registry
            .snapshot()
            .into_iter()
            .map(|(id, mut run)| {
                if let Some(usage) = usage.get(&id) {
                    run.slots = usage.slots;
                    run.share = usage.share;
                }
                run
            })
            .collect()
    }

    /// Counts a client as served until the guard drops; also says whether
    /// the daemon is retiring (then it starts no new run).
    fn serving(&self) -> (Serving<'_>, bool) {
        let mut lifecycle = self.lifecycle();
        lifecycle.serving += 1;
        (Serving(self), lifecycle.retiring)
    }

    /// Takes no more runs, and exits once the runs in flight are served.
    fn retire(&self) {
        let mut lifecycle = self.lifecycle();
        lifecycle.retiring = true;
        self.exit_if_retired(&lifecycle);
    }

    /// Once retired with no run left, has the main thread exit: a
    /// connection of the daemon's own wakes it from its wait for the next
    /// client. When none can be made, exits here.
    fn exit_if_retired(&self, lifecycle: &Lifecycle) {
        if lifecycle.retiring
            && lifecycle.serving == 0
            && !self.exiting.swap(true, Ordering::SeqCst)
        {
            self.log.write("retired");
            match ipc::connect(&self.endpoint, Instant::now() + WAKE_WAIT) {
                Ok(waker) => {
                    *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(waker)
                }
                Err(_) => std::process::exit(0),
            }
        }
    }

    /// Exits, from the main thread, which owns `listener`. First it stops
    /// listening, so that no client connects to a daemon that will not
    /// answer, and releases the lock, so that the next client starts a new
    /// daemon at once: the process may take a while to be gone, and holds
    /// its endpoint and the lock until then (seen to take over 10 s on a
    /// loaded host).
    fn exit(&self, listener: ipc::Listener) -> ! {
        drop(listener);
        let _ = self.lock.unlock();
        std::process::exit(0)
    }

    /// Its owner, process `owner` ([`owner`]), is gone: kills the runs in
    /// flight, releases the lock and exits.
    fn orphaned(&self, owner: u32) -> ! {
        self.log.write(&format!(
            "its owner, pid {owner}, is gone; cancelling the runs in flight and exiting"
        ));
        for (id, _) in self.registry.snapshot() {
            if let Ok(Some(tree)) = self.registry.cancel(id) {
                let _ = tree.kill();
            }
        }
        let _ = self.lock.unlock();
        std::process::exit(0)
    }

    fn lifecycle(&self) -> MutexGuard<'_, Lifecycle> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A run being served: the daemon does not retire before it drops.
struct Serving<'a>(&'a Daemon);

impl Drop for Serving<'_> {
    fn drop(&mut self) {
        let mut lifecycle = self.0.lifecycle();
        lifecycle.serving -= 1;
        self.0.exit_if_retired(&lifecycle);
    }
}

/// The exit code clients of a run cancelled before it started exit with.
const NEVER_STARTED: i32 = 1;

/// Release protection before asking the accountant to retry, on every exit
/// path, including cancellation, a reused verdict and a spawn failure.
struct RuntimeActivity<'a> {
    guard: Option<File>,
    accounts: &'a accounts::Accounts,
}

impl Drop for RuntimeActivity<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.accounts.runtime_ended();
    }
}

/// A run being executed: what it runs as, and where its output goes.
#[derive(Clone, Copy)]
struct Inputs<'a> {
    request: &'a RunRequest,
    /// The validated epoch, owned through the entire logical run.
    epoch: Option<&'a crate::dependencies::Epoch>,
}

/// A phase of a run, sharing its immutable inputs.
struct Execution<'a, 'r> {
    request: &'a RunRequest,
    /// Also acquired by every phase's guardian before its child starts.
    epoch: Option<&'a crate::dependencies::Epoch>,
    id: RunId,
    /// `CARGO_MAKEFLAGS` for its processes: its run pool.
    makeflags: &'a OsStr,
    /// Its clients follow it here.
    live: &'a Live,
    record: &'a mut Recorder<'r>,
}

/// The test slots a test run asks for: as many as the threads asked for,
/// at most its share (whole slots, at least one).
fn test_slots_wanted(asked: Option<u32>, share: f64) -> u32 {
    // A share is at most the pool's size, which fits a u32.
    let share = share.floor().max(1.0) as u32;
    asked.map_or(share, |asked| asked.clamp(1, share))
}

/// The header line of run `id`'s test phase.
fn test_header(id: RunId, slots: u32, share: f64, waited: Duration) -> String {
    let share = status::slots(share);
    let waited = waited.as_secs_f64();
    match slots {
        0 => format!(
            "cargo-heli: {id}: testing on 1 thread, no pool slot free \
             (share {share}), waited {waited:.1} s\n"
        ),
        1 => format!(
            "cargo-heli: {id}: testing on 1 thread, 1 slot (share {share}), \
             waited {waited:.1} s\n"
        ),
        slots => format!(
            "cargo-heli: {id}: testing on {slots} threads, {slots} slots \
             (share {share}), waited {waited:.1} s\n"
        ),
    }
}

/// The event that tells how run `id`'s loans went: how many slots were lent
/// to its pool and taken back, the most it used, and how long it waited for
/// a lent slot (at most: from when its pool was last seen with a free
/// slot), in milliseconds.
fn loans_event(id: RunId, loans: &Loans) -> serde_json::Value {
    let millis = |wait: Duration| (wait.as_secs_f64() * 10_000.0).round() / 10.0;
    let (median, longest) = loans.waits().map_or((None, None), |(median, longest)| {
        (Some(millis(median)), Some(millis(longest)))
    });
    serde_json::json!({
        "run": id.to_string(),
        "loans": loans.count,
        "taken_back": loans.reclaimed,
        "most": loans.most,
        "wait_median_ms": median,
        "wait_longest_ms": longest,
    })
}

/// What the client of `request` reads besides a run's output and end: what
/// the run waits for, if it knows run classes (a client older than them
/// sends none), and when its command started, if it says so.
fn reads(request: &RunRequest) -> live::Reads {
    live::Reads {
        waiting: request.class.is_some(),
        spawned: request.reads_spawned,
    }
}

/// Whether `request` comes from inside another heli run: its client runs
/// under heli, so the run's depth (`HELI_DEPTH`, the client's plus one) is
/// above 1.
fn nested(request: &RunRequest) -> bool {
    request
        .var(nesting::VAR)
        .and_then(|depth| depth.to_str()?.trim().parse::<u32>().ok())
        .is_some_and(|depth| depth > 1)
}

/// The exit code of a run that ended with `status`, and on Unix the signal
/// that killed it (the code is then 128 plus the signal).
fn exited(status: std::process::ExitStatus) -> (i32, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (128 + signal, Some(signal));
        }
    }
    (status.code().unwrap_or(1), None)
}

/// The daemon's own log file; clones write to the same file.
#[derive(Clone)]
struct Log {
    file: Option<Arc<Mutex<File>>>,
}

impl Log {
    fn open(locations: &Locations) -> Self {
        let file = File::options()
            .create(true)
            .append(true)
            .open(locations.log_file())
            .ok();
        Self {
            file: file.map(|file| Arc::new(Mutex::new(file))),
        }
    }

    fn write(&self, line: &str) {
        if let Some(file) = &self.file {
            let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = writeln!(file, "{} [{}] {line}", clock::now(), std::process::id());
        }
    }
}

#[cfg(test)]
mod connection_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Connection, MAX_CONNECTIONS};

    #[test]
    fn connections_are_bounded_and_freed_when_dropped() {
        let open = Arc::new(AtomicUsize::new(0));
        let held: Vec<Connection> = (0..MAX_CONNECTIONS)
            .map(|_| Connection::claim(&open).expect("a place"))
            .collect();
        assert!(Connection::claim(&open).is_none(), "all places are taken");
        drop(held);
        assert_eq!(open.load(Ordering::SeqCst), 0);
        assert!(Connection::claim(&open).is_some());
    }
}
