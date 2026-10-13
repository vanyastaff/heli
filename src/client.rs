//! The client side of `cargo heli`: hands each run to the daemon and acts
//! as the command would have.
//!
//! - **Reaching the daemon.** Connect to the endpoint; when nothing listens
//!   there, start a daemon (`crate::launch`) and connect as soon as it
//!   listens. Then the handshake (`crate::protocol`): versions must match.
//! - **A run.** Send the command, the working directory and the whole
//!   environment (`HELI_DEPTH` one deeper); print the run's stdout and
//!   stderr to the same streams as they arrive; exit with the run's code
//!   (on Unix, die of the run's signal). A command that cannot be started
//!   is reported as direct execution reports it (exit 127 or 126).
//! - **Fallback.** Whenever the daemon cannot take the run (it does not
//!   start, cannot be reached, does not answer, speaks another protocol
//!   version, or is retiring), print one warning line and run the command
//!   directly (`crate::direct`): a failure of heli never fails a run. Once
//!   the daemon has said the run started, there is no fallback: the command
//!   may already have run, so losing the daemon is an error (exit 1).
//! - **Status.** Ask the daemon, or report that none runs; never start one.

use std::convert::Infallible;
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cli::Invocation;
use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::protocol::{self, Channel, Frame, LogRequest, Reply, Request, RunRequest, Text};
use crate::runlog::{self, select::Select};
use crate::status::Status;
use crate::{clock, direct, environment, launch, nesting};

/// How long a daemon may take to answer the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Exit code when the connection to the daemon breaks during a run.
const LOST: i32 = 1;

/// Exit code when `log` cannot print the record.
const FAILED: i32 = 1;

/// Runs `invocation` through the daemon, or directly when it cannot; this
/// process's nesting depth is `depth`.
pub fn run(invocation: Invocation, depth: u32) -> ! {
    let child_depth = OsString::from((depth + 1).to_string());
    let reason = match Locations::resolve() {
        Err(error) => error,
        Ok(locations) => match reach(&locations, Some(depth)) {
            Ok(Some(stream)) => match submit(stream, &invocation, &child_depth) {
                Ok(never) => match never {},
                Err(reason) => reason,
            },
            Ok(None) => "no heli daemon answered".to_owned(),
            Err(reason) => reason,
        },
    };
    eprintln!("cargo-heli: warning: {reason}; running the command directly");
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .env(nesting::VAR, child_depth);
    direct::run(command)
}

/// Prints the daemon's status, as JSON or as text.
pub fn status(json: bool) -> ! {
    let status = query_status().unwrap_or_else(|reason| {
        eprintln!("cargo-heli: warning: {reason}");
        Status::without_daemon()
    });
    if json {
        let text = serde_json::to_string(&status).expect("BUG: the status serializes");
        println!("{text}");
    } else {
        print!("{}", status.to_text(clock::now()));
    }
    std::process::exit(0)
}

/// Prints the part of run `run`'s record that `select` picks: stdout on
/// stdout, stderr on stderr. The daemon sends it; when none answers, this
/// client reads the record itself (and starts no daemon).
pub fn log(run: &str, select: &Select) -> ! {
    let locations = Locations::resolve().unwrap_or_else(|error| {
        eprintln!("cargo-heli: {error}");
        std::process::exit(FAILED)
    });
    let reason = match reach(&locations, None) {
        Ok(Some(stream)) => match ask_log(stream, run, select) {
            Ok(never) => match never {},
            Err(reason) => Some(reason),
        },
        Ok(None) => None,
        Err(reason) => Some(reason),
    };
    if let Some(reason) = reason {
        eprintln!("cargo-heli: warning: {reason}; reading the run log directly");
    }
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let read = runlog::read(&locations.run_logs(), run, select, &mut |channel, bytes| {
        match channel {
            Channel::Stdout => print(&mut stdout, bytes),
            Channel::Stderr => print(&mut stderr, bytes),
        }
        Ok(())
    });
    let _ = stdout.flush();
    if let Err(error) = read {
        eprintln!("cargo-heli: {error}");
        std::process::exit(FAILED)
    }
    std::process::exit(0)
}

/// Asks the daemon for the record, prints it as it arrives, and exits.
/// Returns only when the daemon sent none of it, with the reason.
fn ask_log(mut stream: Stream, run: &str, select: &Select) -> Result<Infallible, String> {
    let request = Request::Log(LogRequest {
        run: run.to_owned(),
        select: select.clone(),
    });
    protocol::write_message(&mut stream, &request)
        .map_err(|error| format!("cannot ask the heli daemon: {error}"))?;
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let mut printed = false;
    loop {
        let failure = match protocol::read_frame::<Reply>(&mut stream) {
            Ok(Frame::Output(Channel::Stdout, bytes)) => {
                print(&mut stdout, &bytes);
                printed = true;
                continue;
            }
            Ok(Frame::Output(Channel::Stderr, bytes)) => {
                print(&mut stderr, &bytes);
                printed = true;
                continue;
            }
            Ok(Frame::Message(Reply::LogEnd)) => {
                let _ = stdout.flush();
                std::process::exit(0)
            }
            Ok(Frame::Message(Reply::LogFailed { message })) => {
                let _ = stdout.flush();
                eprintln!("cargo-heli: {message}");
                std::process::exit(FAILED)
            }
            Ok(Frame::Message(Reply::Unsupported)) => unsupported(&mut stream),
            Ok(Frame::Message(other)) => format!("the heli daemon answered {other:?} to log"),
            Err(error) => format!("the heli daemon did not answer: {error}"),
        };
        if !printed {
            return Err(failure);
        }
        let _ = stdout.flush();
        eprintln!("cargo-heli: lost the heli daemon while printing {run}: {failure}");
        std::process::exit(FAILED)
    }
}

fn query_status() -> Result<Status, String> {
    let locations = Locations::resolve()?;
    let Some(mut stream) = reach(&locations, None)? else {
        return Ok(Status::without_daemon());
    };
    let asked = protocol::write_message(&mut stream, &Request::Status)
        .and_then(|()| protocol::read_message::<Reply>(&mut stream));
    match asked {
        Ok(Reply::Status(status)) => Ok(status),
        Ok(other) => Err(format!("the heli daemon answered {other:?} to status")),
        Err(error) => Err(format!("the heli daemon did not answer: {error}")),
    }
}

/// A connection to the daemon after a matching handshake. With `start`
/// (this client's depth), a daemon is started when none listens; without,
/// that case is `Ok(None)`. The error is the reason to fall back.
fn reach(locations: &Locations, start: Option<u32>) -> Result<Option<Stream>, String> {
    let stream = match ipc::connect(&locations.endpoint) {
        Ok(stream) => stream,
        Err(error) if ipc::is_absent(&error) => match start {
            None => return Ok(None),
            Some(depth) => start_daemon(locations, depth)?,
        },
        Err(error) => return Err(launch::StartError::Unreachable(error).describe(locations)),
    };
    handshake(stream).map(Some)
}

/// Starts a daemon and connects to it, or to the one that won a race to
/// start.
fn start_daemon(locations: &Locations, depth: u32) -> Result<Stream, String> {
    locations.create_state_dir().map_err(|error| {
        format!(
            "cannot create the heli state directory {}: {error}",
            locations.state_dir.display()
        )
    })?;
    let mut launched = launch::start(locations, depth)
        .map_err(|error| format!("cannot start the heli daemon: {error}"))?;
    launch::wait(locations, &mut launched).map_err(|error| error.describe(locations))
}

/// Exchanges versions, within [`HANDSHAKE_TIMEOUT`]. On a mismatch, asks an
/// older daemon to retire.
fn handshake(stream: Stream) -> Result<Stream, String> {
    let (sender, receiver) = mpsc::channel();
    // A thread, so a daemon that never answers cannot hold the client; on
    // timeout it is abandoned with the connection.
    std::thread::spawn(move || {
        let mut stream = stream;
        let version =
            protocol::write_hello(&mut stream).and_then(|()| protocol::read_hello(&mut stream));
        let _ = sender.send(version.map(|version| (stream, version)));
    });
    match receiver.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(Ok((stream, version))) if version == protocol::VERSION => Ok(stream),
        Ok(Ok((mut stream, version))) => {
            let older = version < protocol::VERSION;
            if older {
                let _ = stream.write_all(&[protocol::RETIRE]);
                let _ = stream.flush();
            }
            Err(format!(
                "the heli daemon speaks protocol {version} and this cargo-heli speaks {}{}",
                protocol::VERSION,
                if older {
                    " (the daemon exits once idle)"
                } else {
                    ""
                }
            ))
        }
        Ok(Err(error)) => Err(format!("the heli daemon did not answer: {error}")),
        Err(_) => Err(format!(
            "the heli daemon did not answer within {} s",
            HANDSHAKE_TIMEOUT.as_secs()
        )),
    }
}

/// The daemon answered that it does not know the request, so it is older
/// than this client: as after a version mismatch, asks it to retire.
/// Returns the reason to do without it.
fn unsupported(stream: &mut Stream) -> String {
    let _ = stream.write_all(&[protocol::RETIRE]);
    let _ = stream.flush();
    "the heli daemon is older than this cargo-heli and does not support the request \
     (the daemon exits once idle)"
        .to_owned()
}

/// Hands the run to the daemon and follows it to its end. Returns only when
/// the daemon did not start it, with the reason.
fn submit(
    mut stream: Stream,
    invocation: &Invocation,
    child_depth: &OsString,
) -> Result<std::convert::Infallible, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the working directory: {error}"))?;
    let request = RunRequest {
        cwd: Text(cwd.into()),
        argv: std::iter::once(&invocation.program)
            .chain(&invocation.args)
            .map(|arg| Text(arg.clone()))
            .collect(),
        env: environment::current_with(&[(nesting::VAR, child_depth.clone())])
            .into_iter()
            .map(|(name, value)| (Text(name), Text(value)))
            .collect(),
    };
    protocol::write_message(&mut stream, &Request::Run(request))
        .map_err(|error| format!("cannot hand the run to the heli daemon: {error}"))?;
    match protocol::read_message::<Reply>(&mut stream) {
        Ok(Reply::Started { run }) => follow(stream, invocation, &run, Instant::now()),
        Ok(Reply::Refused { reason }) => Err(reason),
        Ok(Reply::Unsupported) => Err(unsupported(&mut stream)),
        Ok(other) => Err(format!("the heli daemon answered {other:?} to a run")),
        Err(error) => Err(format!("the heli daemon did not take the run: {error}")),
    }
}

/// Prints run `run`'s output as it arrives, then the footer, and exits as
/// the run did; it started at `started`.
fn follow(mut stream: Stream, invocation: &Invocation, run: &str, started: Instant) -> ! {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    // Whether the run's stderr ends a line, so the footer starts one.
    let mut line_ended = true;
    loop {
        match protocol::read_frame::<Reply>(&mut stream) {
            Ok(Frame::Output(Channel::Stdout, bytes)) => print(&mut stdout, &bytes),
            Ok(Frame::Output(Channel::Stderr, bytes)) => {
                print(&mut stderr, &bytes);
                line_ended = bytes.last().is_none_or(|&byte| byte == b'\n');
            }
            Ok(Frame::Message(Reply::Exited { code, signal })) => {
                let _ = stdout.flush();
                let ending = match signal {
                    Some(signal) => format!("signal {signal}"),
                    None => format!("exit {code}"),
                };
                let _ = writeln!(
                    stderr,
                    "{}cargo-heli: {run} {ending} in {:.1} s (output: cargo heli log {run})",
                    if line_ended { "" } else { "\n" },
                    started.elapsed().as_secs_f64()
                );
                drop((stdout, stderr));
                exit_as(code, signal)
            }
            Ok(Frame::Message(Reply::SpawnFailed { not_found, message })) => {
                drop((stdout, stderr));
                direct::report_spawn_failure(&invocation.program, not_found, &message)
            }
            Ok(Frame::Message(other)) => {
                lost(run, &format!("unexpected message {other:?}"), stdout)
            }
            Err(error) => lost(run, &error.to_string(), stdout),
        }
    }
}

fn lost(run: &str, error: &str, mut stdout: io::StdoutLock<'_>) -> ! {
    let _ = stdout.flush();
    eprintln!("cargo-heli: lost the heli daemon during run {run}: {error}");
    std::process::exit(LOST)
}

/// Writes the run's output; a reader that has gone away does not stop the
/// run, so errors only cost the output.
fn print(out: &mut impl Write, bytes: &[u8]) {
    let written = out.write_all(bytes);
    // A Windows console accepts only UTF-8: show other bytes lossily.
    if matches!(&written, Err(error) if error.kind() == io::ErrorKind::InvalidData) {
        let _ = out.write_all(String::from_utf8_lossy(bytes).as_bytes());
    }
    let _ = out.flush();
}

/// Exits as the run did: with its code, or on Unix of its signal.
fn exit_as(code: i32, signal: Option<i32>) -> ! {
    #[cfg(unix)]
    if let Some(number) = signal {
        unsafe extern "C" {
            fn signal(signal: i32, handler: usize) -> usize;
            fn raise(signal: i32) -> i32;
        }
        const SIG_DFL: usize = 0;
        // SAFETY: restores the default action, then raises the signal; if
        // the process survives (the signal is blocked), it exits below.
        unsafe {
            signal(number, SIG_DFL);
            raise(number);
        }
    }
    #[cfg(not(unix))]
    let _ = signal;
    std::process::exit(code)
}
