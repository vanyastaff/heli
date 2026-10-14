//! `cargo heli daemon`: the per-user daemon that executes runs for clients.
//!
//! A client starts it on demand (`crate::launch`); it is never a service.
//! At start it takes the lock in the state directory (one daemon per user,
//! and on Windows per logon session) and listens on the owner-only endpoint
//! (`crate::ipc`). Each connection gets a thread, which speaks
//! `crate::protocol`:
//!
//! - **run**: look its key up in the result store ([`results`], keyed by
//!   [`key`]), admit it to the registry ([`registry`]), tell the client it
//!   started, look its key up again, start its record in the run log
//!   (`crate::runlog`), wait for its turn at the worktree lock (telling the
//!   client for which runs), execute it ([`execute`]), record it in the
//!   result store if it passed, take it out of the registry, report its
//!   exit. `Daemon::run` is the one sequence every run goes through, and
//!   the place for what happens around a run (admission, waits, recording);
//! - **status**: describe the daemon and the registry's runs;
//! - **log**: send part of a run's record;
//! - a request it does not know: answer that it is unsupported;
//! - **retire**, from a newer client (of another protocol version, or with a
//!   request this daemon does not know): accept no more runs and exit once
//!   the runs in flight have reported their exits.
//!
//! What the daemon owns for its whole life (the lock, then the host pool,
//! `crate::pool`, and the run log, `crate::runlog`, both as configured,
//! and on Unix the pool's [`warden`]) is set up in [`main`]. The daemon
//! runs until it is killed or retired. Its own log (start, end, errors)
//! is `daemon.log` in the state directory (`daemon-<session>.log` on
//! Windows), since it has no terminal.

pub mod accounts;
pub mod execute;
pub mod key;
pub mod registry;
pub mod results;
pub mod warden;

use std::fs::{File, TryLockError};
use std::io::Write;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::class::Class;
use crate::config::Config;
use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::pool::{self, Pool};
use crate::protocol::{self, Channel, LogRequest, Reply, Request, RunRequest};
use crate::runlog::{self, Limits, RunLog};
use crate::status::{self, Status};
use crate::{clock, nesting, worktree};

use registry::Registry;
use results::Admitted;

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
    #[cfg(unix)]
    let _warden = start_warden(&locations, &log);
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
        accounts: accounts::Accounts::new(
            config.store.clone(),
            locations.state_dir.clone(),
            log.clone(),
        ),
        registry: Registry::new(&locations.run_counter()),
        pool,
        runlog: RunLog::new(locations.run_logs(), Limits::configured(&config.log)),
        results: results::Store::new(&locations.state_dir),
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

/// Starts the pool's warden ([`warden`]); what it returns must live as
/// long as the daemon. A daemon without a warden still serves.
#[cfg(unix)]
fn start_warden(locations: &Locations, log: &Log) -> Option<std::io::PipeWriter> {
    match warden::start(locations) {
        Ok((alive, pid)) => {
            log.write(&format!("pool warden: pid {pid}"));
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
    pool: Pool,
    runlog: RunLog,
    results: results::Store,
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

    /// Executes `request` for the client on `stream`.
    fn run(&self, mut stream: Stream, request: RunRequest) {
        let Some(serving) = self.admit() else {
            let reason = "the heli daemon is shutting down".to_owned();
            let _ = protocol::write_message(&mut stream, &Reply::Refused { reason });
            return;
        };
        // Before the run waits for anything.
        if let Some(passed) = self.results.before(&request) {
            let _ = protocol::write_message(&mut stream, &passed.reply());
            return;
        }
        let id = self.registry.admit(
            worktree::of(&execute::cwd(&request)),
            execute::argv(&request),
            request.class.unwrap_or(Class::Exec),
            nested(&request),
        );
        // Nothing starts unless the client knows it started: a client that
        // never hears of it runs the command itself.
        let started = Reply::Started {
            run: id.to_string(),
        };
        if protocol::write_message(&mut stream, &started).is_err() {
            self.registry.finish(id);
            return;
        }
        self.accounts
            .learn(&request, request.class.unwrap_or(Class::Exec));
        // Once admitted, after any wait (which belongs above): another run
        // may have passed with the same key meanwhile.
        let key = match self.results.admitted(&request) {
            Admitted::Passed(passed) => {
                self.registry.finish(id);
                let _ = protocol::write_message(&mut stream, &passed.reply());
                return;
            }
            Admitted::Run { key, note } => {
                if let Some(note) = note {
                    self.note(&mut stream, &note);
                }
                key
            }
        };
        let mut record = self.runlog.record(&id.to_string());
        // The worktree lock: tell the client which runs this one waits for,
        // if any (a client older than run classes could not read it). A
        // client gone meanwhile does not stop the run.
        self.registry.wait_for_worktree(id, |runs| {
            if request.class.is_some() {
                let _ = protocol::write_message(&mut stream, &Reply::Waiting { runs });
            }
        });
        let reply = match execute::spawn(&request, id, &self.pool.makeflags()) {
            Err(error) => Reply::SpawnFailed {
                not_found: error.kind() == std::io::ErrorKind::NotFound,
                message: error.to_string(),
            },
            Ok(child) => match execute::pump(child, &mut stream, &mut record) {
                Ok(status) => exited(status),
                Err(error) => {
                    self.log.write(&format!("cannot wait for {id}: {error}"));
                    Reply::Exited {
                        code: 1,
                        signal: None,
                    }
                }
            },
        };
        if let Some(key) = &key {
            let passed = matches!(
                reply,
                Reply::Exited {
                    code: 0,
                    signal: None
                }
            );
            if let Some(note) = self.results.after(&request, key, id, passed) {
                self.note(&mut stream, &note);
            }
        }
        if let Some(error) = record.take_error() {
            self.log.write(&format!("cannot record {id}: {error}"));
        }
        drop(record);
        self.registry.finish(id);
        let _ = protocol::write_message(&mut stream, &reply);
        // The reply is out before a retiring daemon may exit.
        drop(stream);
        drop(serving);
    }

    /// Tells the client of a run why the result store did not do its part,
    /// on the run's stderr.
    fn note(&self, stream: &mut Stream, note: &str) {
        let line = format!("cargo-heli: note: {note}\n");
        let _ = protocol::write_output(stream, Channel::Stderr, line.as_bytes());
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

    /// Counts a run as served until the guard drops; `None` when retiring.
    fn admit(&self) -> Option<Serving<'_>> {
        let mut lifecycle = self.lifecycle();
        if lifecycle.retiring {
            return None;
        }
        lifecycle.serving += 1;
        Some(Serving(self))
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

/// Whether `request` comes from inside another heli run: its client runs
/// under heli, so the run's depth (`HELI_DEPTH`, the client's plus one) is
/// above 1.
fn nested(request: &RunRequest) -> bool {
    request
        .env
        .iter()
        .find(|(name, _)| name.0 == nesting::VAR)
        .and_then(|(_, depth)| depth.0.to_str()?.trim().parse::<u32>().ok())
        .is_some_and(|depth| depth > 1)
}

/// The reply for a run that ended with `status`.
fn exited(status: std::process::ExitStatus) -> Reply {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Reply::Exited {
                code: 128 + signal,
                signal: Some(signal),
            };
        }
    }
    Reply::Exited {
        code: status.code().unwrap_or(1),
        signal: None,
    }
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
