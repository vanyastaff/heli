//! `cargo heli daemon`: the per-user daemon that executes runs for clients.
//!
//! A client starts it on demand (`crate::launch`); it is never a service.
//! At start it takes the lock in the state directory (one daemon per user,
//! and on Windows per logon session) and listens on the owner-only endpoint
//! (`crate::ipc`). Each connection gets a thread, which speaks
//! `crate::protocol`:
//!
//! - **run**: admit it to the registry ([`registry`]), tell the client it
//!   started, execute it ([`execute`]), take it out of the registry, report
//!   its exit. `Daemon::run` is the one sequence every run goes through,
//!   and the place for what happens around a run (admission, waits,
//!   recording);
//! - **status**: describe the daemon and the registry's runs;
//! - **retire**, from a newer client: accept no more runs and exit once the
//!   runs in flight have reported their exits.
//!
//! What the daemon owns for its whole life (the lock, then the host pool,
//! `crate::pool`, sized by the configuration) is set up in [`main`]. The
//! daemon runs until it is killed or retired. Its own log (start, end,
//! errors) is `daemon.log` in the state directory (`daemon-<session>.log` on
//! Windows), since it has no terminal.

pub mod execute;
pub mod registry;

use std::fs::{File, TryLockError};
use std::io::Write;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::pool::broker::{Broker, Loans};
use crate::pool::{self, Pool};
use crate::protocol::{self, Reply, Request, RunRequest};
use crate::status::{self, Status};
use crate::{clock, worktree};

use registry::Registry;

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
    let pool = Arc::new(open_pool(&locations, &log));
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
        registry: Registry::new(&locations.run_counter()),
        broker: Broker::start(Arc::clone(&pool)),
        pool,
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
fn open_pool(locations: &Locations, log: &Log) -> Pool {
    let config = Config::load(&locations.config).unwrap_or_else(|error| fail(Some(log), &error));
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
    pool: Arc<Pool>,
    /// Lends the host pool's slots to the runs' own pools.
    broker: Arc<Broker>,
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
            let mut byte = [0u8];
            if matches!(std::io::Read::read(&mut stream, &mut byte), Ok(1))
                && byte[0] == protocol::RETIRE
            {
                self.log.write(&format!(
                    "a client of protocol {version} asked this daemon to retire"
                ));
                self.retire();
            }
            return;
        }
        match protocol::read_message::<Request>(&mut stream) {
            Ok(Request::Status) => {
                let _ = protocol::write_message(&mut stream, &Reply::Status(self.status()));
            }
            Ok(Request::Run(request)) => self.run(stream, request),
            Err(_) => {}
        }
    }

    /// Executes `request` for the client on `stream`.
    fn run(&self, mut stream: Stream, request: RunRequest) {
        let Some(serving) = self.admit() else {
            let reason = "the heli daemon is shutting down".to_owned();
            let _ = protocol::write_message(&mut stream, &Reply::Refused { reason });
            return;
        };
        let id = self.registry.admit(
            worktree::of(&execute::cwd(&request)),
            execute::argv(&request),
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
        let (makeflags, error) = self.broker.open(id);
        if let Some(error) = error {
            self.log.write(&format!(
                "{id}: no run pool ({error}); it draws from the host pool"
            ));
        }
        let reply = match execute::spawn(&request, id, &makeflags) {
            Err(error) => Reply::SpawnFailed {
                not_found: error.kind() == std::io::ErrorKind::NotFound,
                message: error.to_string(),
            },
            Ok(child) => match execute::pump(child, &mut stream) {
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
        if let Some(loans) = self.broker.close(id) {
            self.log
                .write(&format!("{id}: run pool: {}", describe(&loans)));
        }
        self.registry.finish(id);
        let _ = protocol::write_message(&mut stream, &reply);
        // The reply is out before a retiring daemon may exit.
        drop(stream);
        drop(serving);
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
            runs: self.runs(),
        }
    }

    /// The runs in flight, with the slots each uses.
    fn runs(&self) -> Vec<status::Run> {
        let usage = self.broker.usage();
        let mut runs = self.registry.snapshot();
        for run in &mut runs {
            if let Some(usage) = usage.iter().find(|(id, _)| id.to_string() == run.id) {
                run.slots = usage.1.slots;
                run.share = usage.1.share;
            }
        }
        runs
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

/// How a run's loans went, for the log: how many, the most slots it used,
/// and how long it waited for a lent slot (at most: from when its pool was
/// last seen with a free slot).
fn describe(loans: &Loans) -> String {
    let millis = |wait: Duration| wait.as_secs_f64() * 1000.0;
    match loans.waits() {
        None => "no loans".to_owned(),
        Some((median, longest)) => format!(
            "{} loans, {} taken back, at most {} slots in use; \
             wait for a lent slot: median {:.1} ms, longest {:.1} ms",
            loans.count,
            loans.reclaimed,
            loans.most,
            millis(median),
            millis(longest)
        ),
    }
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
