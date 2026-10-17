//! Executing one run for a client: the command as the client would have
//! started it, its output streamed back as it comes.
//!
//! The run is the daemon's own child, started in the client's working
//! directory with exactly the client's environment (nothing of the
//! daemon's), and with no stdin, as the first process of a process tree of
//! its own (`super::tree`); what heli adds to every run's environment
//! belongs in [`spawn`]: the pool it draws from ([`MAKEFLAGS_VAR`]) and its
//! id ([`RUN_VAR`]). Its stdout and stderr are pipes the daemon drains
//! ([`pump`]), the one place all of a run's output passes: into the run log
//! (`crate::runlog`), and from there to the clients following the run
//! (`super::live`). No client is written to here, so none can hold up the
//! run: one that stops reading, or goes away, misses nothing in the record.
//!
//! **When a run is over** is when its process has exited, as for a command
//! the client waited for itself: processes it left behind may keep its
//! stdout open, so the daemon takes what is left in the pipes for a short
//! grace period, then reports the exit and keeps draining the pipes into the
//! void until they close.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::live::Live;
use super::tree::{self, Tree};
use crate::protocol::{Channel, RunRequest};
use crate::run_id::RunId;
use crate::runlog::Recorder;

/// How long output may still arrive after the run's process has exited.
const GRACE: Duration = Duration::from_millis(500);

/// How much output one frame carries at most.
const CHUNK: usize = 64 * 1024;

/// Points cargo, rustc and build scripts at the jobserver they draw from,
/// replacing the client's own value.
pub const MAKEFLAGS_VAR: &str = "CARGO_MAKEFLAGS";

/// The run's id: tools that keep a host lock of their own can skip it under
/// heli.
pub const RUN_VAR: &str = "HELI_RUN";

/// Starts run `id`'s process with the command line `argv` (the request's,
/// or one a phase of the run derives from it), drawing from the jobserver
/// `makeflags` names, as the first of a process tree of its own
/// (`super::tree`).
pub fn spawn(
    request: &RunRequest,
    argv: &[OsString],
    id: RunId,
    makeflags: &OsStr,
    roots: &[PathBuf],
) -> io::Result<(Child, Tree)> {
    let cwd = request.cwd();
    keep_sccache_server(request, &cwd);
    let mut argv = argv.iter().cloned();
    let program = argv.next().unwrap_or_default();
    let resolved = resolve(&cwd, program);
    let guarded = !roots.is_empty() || request.dependency_epoch.is_some();
    let mut command = if !guarded {
        let mut command = Command::new(&resolved);
        command.args(argv);
        command
    } else {
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("lease-run");
        if let Some(epoch) = &request.dependency_epoch {
            command.arg("--dependency-epoch").arg(&epoch.0);
        }
        for root in roots {
            command.arg("--root").arg(root);
        }
        command.arg("--").arg(&resolved).args(argv);
        command
    };
    command
        .current_dir(&cwd)
        .env_clear()
        .envs(request.env.iter().map(|(name, value)| (&name.0, &value.0)))
        .env(MAKEFLAGS_VAR, makeflags)
        .env(RUN_VAR, id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, tree) = tree::spawn(&mut command)?;
    if guarded {
        let handshake = crate::protocol::read_message::<crate::store::runner::Handshake>(
            child
                .stdout
                .as_mut()
                .expect("the lease runner has piped stdout"),
        );
        let failure = match handshake {
            Ok(crate::store::runner::Handshake::Spawned) => return Ok((child, tree)),
            Ok(crate::store::runner::Handshake::SpawnFailed { not_found, message }) => {
                io::Error::new(
                    if not_found {
                        io::ErrorKind::NotFound
                    } else {
                        io::ErrorKind::Other
                    },
                    message,
                )
            }
            Err(error) => error,
        };
        // A malformed handshake must not leave an unobserved command running.
        let _ = tree.kill();
        let _ = child.wait();
        return Err(failure);
    }
    Ok((child, tree))
}

/// How long the daemon waits for `sccache --start-server` to return.
const SURVIVOR_START: Duration = Duration::from_secs(5);

/// Survivors (`DESIGN.md` §5, "Tool profiles"): a compiler cache server that
/// a run's `RUSTC_WRAPPER=sccache` would start inside the run ends with the
/// run's process tree, so cancelling one build cold-starts everyone's cache.
/// The daemon starts it first, outside any run's tree. A server that already
/// runs makes `--start-server` fail, which is the answer wanted; failures
/// never fail the run, whose own sccache starts a server as it would anyway.
fn keep_sccache_server(request: &RunRequest, cwd: &Path) {
    let Some(wrapper) = request.var("RUSTC_WRAPPER").filter(|wrapper| {
        Path::new(wrapper)
            .file_stem()
            .is_some_and(|stem| stem == "sccache")
    }) else {
        return;
    };
    let mut command = Command::new(resolve(cwd, wrapper.to_owned()));
    command
        .arg("--start-server")
        .current_dir(cwd)
        .env_clear()
        .envs(request.env.iter().map(|(name, value)| (&name.0, &value.0)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the daemon has no console to share.
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut starter) = command.spawn() else {
        return;
    };
    let deadline = Instant::now() + SURVIVOR_START;
    while Instant::now() < deadline {
        match starter.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
    let _ = starter.kill();
    let _ = starter.wait();
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

/// Records the run's output in `record`, and adds it to `live` for its
/// clients, until the run is over (see the module docs); returns how it
/// ended.
pub fn pump(mut child: Child, record: &mut Recorder<'_>, live: &Live) -> io::Result<ExitStatus> {
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
                let recorded = record.output(channel, &bytes);
                live.append(channel, bytes, recorded);
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
