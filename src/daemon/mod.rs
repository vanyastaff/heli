//! `cargo heli daemon`: the per-user daemon that executes runs for clients.
//!
//! A client starts it on demand (`crate::launch`); it is never a service.
//! At start it takes the lock in the state directory (one daemon per user,
//! and on Windows per logon session) and listens on the owner-only endpoint
//! (`crate::ipc`). Each connection gets a thread, which speaks
//! `crate::protocol`:
//!
//! - **run**: admit it to the registry ([`registry`]), tell the client it
//!   started, start its record in the run log (`crate::runlog`), execute it
//!   ([`execute`]), take it out of the registry, report its exit.
//!   `Daemon::run` is the one sequence every run goes through, and the
//!   place for what happens around a run (admission, waits, recording);
//! - **status**: describe the daemon and the registry's runs;
//! - **log**: send part of a run's record;
//! - a request it does not know: answer that it is unsupported;
//! - **retire**, from a newer client (of another protocol version, or with a
//!   request this daemon does not know): accept no more runs and exit once
//!   the runs in flight have reported their exits.
//!
//! What the daemon owns for its whole life (the lock, then the host pool,
//! `crate::pool`, and the run log, `crate::runlog`, both as configured) is
//! set up in [`main`]. The daemon runs until it is killed or retired. Its
//! own log (start, end, errors) is `daemon.log` in the state directory
//! (`daemon-<session>.log` on Windows), since it has no terminal.

pub mod execute;
pub mod live;
pub mod registry;

use std::fs::{File, TryLockError};
use std::io::Write;
use std::num::NonZeroU32;
use std::process::Child;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::pool::{self, Pool};
use crate::protocol::{self, LogRequest, Reply, Request, RunRequest};
use crate::runlog::{self, Events, Limits, Recorder, RunLog};
use crate::status::{self, Status};
use crate::{clock, worktree};

use live::Live;
use registry::{Admission, Attached, Command, Registry, RunId, RunIds};

/// The daemon's exit code when another daemon already holds the lock: the
/// client that started it then connects to that one.
pub const ALREADY_RUNNING: i32 = 3;

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
        Err(TryLockError::WouldBlock) => std::process::exit(ALREADY_RUNNING),
        Err(TryLockError::Error(error)) => {
            fail(Some(&log), &format!("cannot take the lock: {error}"))
        }
    }
    let config = Config::load(&locations.config).unwrap_or_else(|error| fail(Some(&log), &error));
    let pool = open_pool(&locations, &config, &log);
    let mut listener = match ipc::Listener::bind(&locations.endpoint) {
        Ok(listener) => listener,
        Err(error) => fail(
            Some(&log),
            &format!("cannot listen at {}: {error}", locations.endpoint.display()),
        ),
    };
    log.write(&format!(
        "started, pid {}, protocol {}, endpoint {}",
        std::process::id(),
        protocol::VERSION,
        locations.endpoint.display()
    ));

    let daemon = Arc::new(Daemon {
        registry: Registry::new(RunIds::new(
            locations.run_counter(),
            locations.run_counter_lock(),
            locations.run_logs(),
        )),
        pool,
        runlog: RunLog::new(locations.run_logs(), Limits::configured(&config.log)),
        lifecycle: Mutex::new(Lifecycle::default()),
        started_at: SystemTime::now(),
        log,
        _lock: lock,
    });
    loop {
        match listener.accept() {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                std::thread::spawn(move || daemon.serve(stream));
            }
            Err(error) => {
                daemon.log.write(&format!("accept failed: {error}"));
                std::thread::sleep(Duration::from_millis(100));
            }
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

struct Daemon {
    registry: Registry,
    pool: Pool,
    runlog: RunLog,
    lifecycle: Mutex<Lifecycle>,
    started_at: SystemTime,
    log: Log,
    /// Held until the process exits: the operating system releases it even
    /// when the daemon is killed.
    _lock: File,
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
                let _ = protocol::write_message(&mut stream, &Reply::Status(self.status()));
            }
            Ok(Request::Run(request)) => self.run(stream, request),
            Ok(Request::Log(request)) => self.log(stream, &request),
            Ok(Request::Cancel(_)) | Err(_) => {
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

    /// Sends the part of a run's record that `request` asks for.
    fn log(&self, mut stream: Stream, request: &LogRequest) {
        let read = runlog::read(
            self.runlog.dir(),
            &request.run,
            &request.select,
            &mut |channel, bytes| protocol::write_output(&mut stream, channel, bytes),
        );
        let reply = match read {
            Ok(()) => Reply::LogEnd,
            Err(error) => Reply::LogFailed {
                message: error.to_string(),
            },
        };
        let _ = protocol::write_message(&mut stream, &reply);
    }

    /// Executes `request` for the client on `stream`, or attaches the
    /// client to the same command in flight.
    fn run(&self, mut stream: Stream, request: RunRequest) {
        let (serving, retiring) = self.serving();
        let worktree = worktree::of(&execute::cwd(&request));
        match self
            .registry
            .admit(worktree, Command::of(&request), retiring)
        {
            Admission::New(id) => self.start(stream, &request, id),
            Admission::Attached(run) => self.attach(stream, run),
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

    /// Follows run `run.id`, already in flight, for the client on `stream`.
    fn attach(&self, mut stream: Stream, run: Attached) {
        let started = Reply::Started {
            run: run.id.to_string(),
            attached: Some(u64::try_from(run.running_for.as_millis()).unwrap_or(u64::MAX)),
        };
        if protocol::write_message(&mut stream, &started).is_err() {
            self.registry.detach(run.id);
            return;
        }
        self.follow(run.id, &run.live, run.events, stream);
    }

    /// Starts run `id` of `request`, and follows it for the client on
    /// `stream`; returns once the run has ended.
    fn start(&self, mut stream: Stream, request: &RunRequest, id: RunId) {
        // Nothing starts unless the client knows it started: a client that
        // never hears of it runs the command itself.
        let started = Reply::Started {
            run: id.to_string(),
            attached: None,
        };
        if protocol::write_message(&mut stream, &started).is_err() {
            self.registry.abandon(id);
            return;
        }
        let record = self.runlog.record(&id.to_string());
        let child = match execute::spawn(request, id, &self.pool.makeflags()) {
            Ok(child) => child,
            Err(error) => {
                drop(record);
                self.registry.abandon(id);
                let failed = Reply::SpawnFailed {
                    not_found: error.kind() == std::io::ErrorKind::NotFound,
                    message: error.to_string(),
                };
                let _ = protocol::write_message(&mut stream, &failed);
                return;
            }
        };
        let live = Live::new(self.runlog.dir().to_path_buf(), id.to_string());
        let events = live.open();
        let live = Arc::new(live);
        self.registry.start(id, Arc::clone(&live));
        // The run goes on whether or not its client follows it to the end.
        std::thread::scope(|scope| {
            scope.spawn(|| self.execute(id, child, record, &live));
            self.follow(id, &live, events, stream);
        });
    }

    /// Runs `child`, the process of run `id`, to its end: records its
    /// output, ends `live` with how it ended.
    fn execute(&self, id: RunId, child: Child, mut record: Recorder<'_>, live: &Live) {
        let (code, signal) = match execute::pump(child, &mut record, live) {
            Ok(status) => exited(status),
            Err(error) => {
                self.log.write(&format!("cannot wait for {id}: {error}"));
                (1, None)
            }
        };
        if let Some(error) = record.take_error() {
            self.log.write(&format!("cannot record {id}: {error}"));
        }
        // Taken out of the registry before the record may be pruned, so a
        // client attaching to it still finds all of it.
        let exit = self.registry.end(id, code, signal);
        drop(record);
        live.end(exit);
    }

    /// Sends run `id`'s output, from `events` on, to the client on
    /// `stream`, until the run ends or the client goes away.
    fn follow(&self, id: RunId, live: &Live, events: Option<Events>, mut stream: Stream) {
        live::follow(live, events, &mut stream);
        drop(stream);
        self.registry.detach(id);
    }

    fn status(&self) -> Status {
        Status {
            schema: status::SCHEMA,
            daemon: Some(status::Daemon {
                pid: std::process::id(),
                protocol: protocol::VERSION,
                started_at: clock::unix_seconds(self.started_at),
            }),
            pool: Some(self.pool.status()),
            runs: self.registry.snapshot(),
        }
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

    fn exit_if_retired(&self, lifecycle: &Lifecycle) {
        if lifecycle.retiring && lifecycle.serving == 0 {
            self.log.write("retired");
            std::process::exit(0);
        }
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

/// The daemon's own log file.
struct Log {
    file: Option<Mutex<File>>,
}

impl Log {
    fn open(locations: &Locations) -> Self {
        let file = File::options()
            .create(true)
            .append(true)
            .open(locations.log_file())
            .ok();
        Self {
            file: file.map(Mutex::new),
        }
    }

    fn write(&self, line: &str) {
        if let Some(file) = &self.file {
            let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = writeln!(file, "{} [{}] {line}", clock::now(), std::process::id());
        }
    }
}
