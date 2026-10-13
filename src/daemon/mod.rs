//! `cargo heli daemon`: the per-user daemon that executes runs for clients.
//!
//! A client starts it on demand (`crate::launch`); it is never a service.
//! At start it takes the lock in the state directory (one daemon per user,
//! and on Windows per logon session) and listens on the owner-only endpoint
//! (`crate::ipc`). Each connection gets a thread, which speaks
//! `crate::protocol`:
//!
//! - **run**: admit it to the registry ([`registry`]), tell the client it
//!   started, wait for its turn at the worktree lock (telling the client
//!   for which runs), execute it ([`execute`]), take it out of the
//!   registry, report its exit. `Daemon::run` is the one sequence every run
//!   goes through, and the place for what happens around a run (admission,
//!   waits, recording);
//! - **status**: describe the daemon and the registry's runs;
//! - **retire**, from a newer client: accept no more runs and exit once the
//!   runs in flight have reported their exits.
//!
//! What the daemon owns for its whole life (now the lock; later the pools)
//! is set up in [`main`]. The daemon runs until it is killed or retired. Its
//! own log (start, end, errors) is `daemon.log` in the state directory
//! (`daemon-<session>.log` on Windows), since it has no terminal.

pub mod execute;
pub mod registry;

use std::fs::{File, TryLockError};
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::protocol::{self, Reply, Request, RunRequest};
use crate::status::{self, Status};
use crate::{clock, nesting, worktree};

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

struct Daemon {
    registry: Registry,
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
            request.class,
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
        // The worktree lock: tell the client which runs this one waits for,
        // if any. A client gone meanwhile does not stop the run.
        self.registry.wait_for_worktree(id, |runs| {
            let _ = protocol::write_message(&mut stream, &Reply::Waiting { runs });
        });
        let reply = match execute::spawn(&request) {
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
