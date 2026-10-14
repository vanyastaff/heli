//! Executing one run for a client: the command as the client would have
//! started it, its output streamed back as it comes.
//!
//! The run is the daemon's own child, started in the client's working
//! directory with exactly the client's environment (nothing of the
//! daemon's), and with no stdin; what heli adds to every run's environment
//! belongs in [`spawn`]: the pool it draws from ([`MAKEFLAGS_VAR`]) and its
//! id ([`RUN_VAR`]). Its stdout and stderr are pipes the daemon forwards
//! frame by frame ([`pump`]), the one place all of a run's output passes;
//! there it is also recorded in the run log (`crate::runlog`).
//!
//! **When a run is over** is when its process has exited, as for a command
//! the client waited for itself: processes it left behind may keep its
//! stdout open, so the daemon forwards what is left in the pipes for a short
//! grace period, then reports the exit and keeps draining the pipes into the
//! void until they close.
//!
//! A client that goes away does not stop the run: the output it misses is
//! still recorded.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::registry::RunId;
use crate::protocol::{self, Channel, RunRequest};
use crate::runlog::Recorder;

/// How long output may still arrive after the run's process has exited.
const GRACE: Duration = Duration::from_millis(500);

/// How much output one frame carries at most.
const CHUNK: usize = 64 * 1024;

/// The run's command line, program first.
pub fn argv(request: &RunRequest) -> Vec<OsString> {
    request.argv.iter().map(|arg| arg.0.clone()).collect()
}

/// The run's working directory.
pub fn cwd(request: &RunRequest) -> PathBuf {
    PathBuf::from(&request.cwd.0)
}

/// Points cargo, rustc and build scripts at the jobserver they draw from,
/// replacing the client's own value.
pub const MAKEFLAGS_VAR: &str = "CARGO_MAKEFLAGS";

/// The run's id: tools that keep a host lock of their own can skip it under
/// heli.
pub const RUN_VAR: &str = "HELI_RUN";

/// Starts run `id`'s process with the command line `argv` (the request's,
/// or one a phase of the run derives from it), drawing from the jobserver
/// `makeflags` names.
pub fn spawn(
    request: &RunRequest,
    argv: &[OsString],
    id: RunId,
    makeflags: &OsStr,
) -> io::Result<Child> {
    let cwd = cwd(request);
    let mut argv = argv.iter().cloned();
    let program = argv.next().unwrap_or_default();
    Command::new(resolve(&cwd, program))
        .args(argv)
        .current_dir(&cwd)
        .env_clear()
        .envs(request.env.iter().map(|(name, value)| (&name.0, &value.0)))
        .env(MAKEFLAGS_VAR, makeflags)
        .env(RUN_VAR, id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// A program given as a relative path (`./script`, `bin\tool`) is relative
/// to the client's working directory, not the daemon's. Bare names are left
/// to the search path.
fn resolve(cwd: &Path, program: OsString) -> PathBuf {
    let program = PathBuf::from(program);
    if program.is_relative() && program.components().count() > 1 {
        cwd.join(program)
    } else {
        program
    }
}

/// What the pump learns.
enum Event {
    Output(Channel, Vec<u8>),
    Closed,
    Exited(io::Result<ExitStatus>),
}

/// Records the run's output in `record` and forwards it to `client` until
/// the run is over (see the module docs), and returns how it ended.
pub fn pump(
    mut child: Child,
    client: &mut impl Write,
    record: &mut Recorder<'_>,
) -> io::Result<ExitStatus> {
    let (events, received) = mpsc::sync_channel(16);
    let mut open = 0;
    for (channel, pipe) in [
        (
            Channel::Stdout,
            child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        ),
        (
            Channel::Stderr,
            child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        ),
    ] {
        if let Some(pipe) = pipe {
            open += 1;
            let events = events.clone();
            std::thread::spawn(move || forward(channel, pipe, events));
        }
    }
    let waiter = events;
    std::thread::spawn(move || {
        let _ = waiter.send(Event::Exited(child.wait()));
    });

    let mut connected = true;
    let mut exit = None;
    let mut deadline: Option<Instant> = None;
    while exit.is_none() || open > 0 {
        let event = match deadline {
            None => received.recv().ok(),
            Some(deadline) => received
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok(),
        };
        match event {
            Some(Event::Output(channel, bytes)) => {
                // Recorded first, so the record is complete when the client
                // hears of the exit, and whether or not a client listens.
                record.output(channel, &bytes);
                if connected && protocol::write_output(client, channel, &bytes).is_err() {
                    connected = false;
                }
            }
            Some(Event::Closed) => open -= 1,
            Some(Event::Exited(status)) => {
                exit = Some(status);
                deadline = Some(Instant::now() + GRACE);
            }
            // The grace period is over: processes the run left behind hold
            // its pipes.
            None => break,
        }
    }
    exit.unwrap_or_else(|| Err(io::Error::other("the run's waiter vanished")))
}

/// Sends what `pipe` yields as events, then [`Event::Closed`]; once nobody
/// listens, reads on until the pipe closes, so the writer never blocks.
fn forward(channel: Channel, mut pipe: Box<dyn Read + Send>, events: mpsc::SyncSender<Event>) {
    let mut buffer = vec![0u8; CHUNK];
    let mut listening = true;
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if listening
                    && events
                        .send(Event::Output(channel, buffer[..read].to_vec()))
                        .is_err()
                {
                    listening = false;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    if listening {
        let _ = events.send(Event::Closed);
    }
}
