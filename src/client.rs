//! The client side of `cargo heli`: hands each run to the daemon and acts
//! as the command would have.
//!
//! - **Reaching the daemon.** Connect to the endpoint; when nothing listens
//!   there, start a daemon (`crate::launch`) and connect as soon as it
//!   listens. Then the handshake (`crate::protocol`): versions must match.
//! - **A run.** Choose the output mode and plan the run for it
//!   (`crate::output`); send the command as planned, the working directory
//!   and the whole environment (`HELI_DEPTH` one deeper, and what the plan
//!   sets); print the run's stdout and stderr to the same streams through
//!   the plan's printer, then its closing line; exit with the run's code
//!   (on Unix, die of the run's signal). A command that cannot be started
//!   is reported as direct execution reports it (exit 127 or 126). A run
//!   that waits for its worktree's lock says so first, on stderr: "waiting
//!   for …", naming the runs it waits for (through the printer, so quiet
//!   mode holds it back with the rest). When the same run is already in
//!   flight (the same command, or for a verify or test run the same key,
//!   in any worktree), the daemon attaches this client to it: one notice
//!   line on stderr (through the printer too) naming the run and how long
//!   it has been going, then the same as for a run of its own, from the
//!   run's first byte.
//! - **A hit.** When the daemon answers that the run's key already passed
//!   (`crate::daemon::results`), say which run passed and when, with the
//!   caveats of reusing a verdict, and exit 0 without running anything.
//! - **Fallback.** Whenever the daemon cannot take the run (it does not
//!   start, cannot be reached, does not answer, speaks another protocol
//!   version, or is retiring), print one warning line and run the command
//!   directly (`crate::direct`), as the user gave it, with its output
//!   untouched: a failure of heli never fails a run. That holds while the
//!   run waits too (for its worktree's lock, or for recycling): a daemon
//!   lost before it says the command has started (`Spawned`) means a direct
//!   run, after one warning. Once the command has started (or, with a
//!   daemon older than `Spawned`, once the run is accepted), there is no
//!   fallback: the command may already have run, so losing the daemon is an
//!   error (exit 1).
//! - **Status.** Ask the daemon, or report that none runs; never start one.
//! - **Ensure.** Reach the daemon, starting it if need be, and nothing else.
//! - **Cancel.** Ask the daemon to kill a run's process tree; without a
//!   daemon there is no run to cancel, and none is started.

use std::convert::Infallible;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cli::Invocation;
use crate::config::Config;
use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::output::{self, Plan, Printer, print};
use crate::protocol::{
    self, CancelRequest, Channel, Frame, LogRequest, Reply, Request, RunRequest, Text,
};
use crate::run_id::RunId;
use crate::runlog::{self, select::Select};
use crate::status::{self, Exit, Status};
use crate::{clock, direct, environment, launch, nesting};

/// How long a daemon may take to answer the handshake, and to take the
/// connection when its endpoint is busy.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Exit code when the connection to the daemon breaks during a run.
const LOST: i32 = 1;

/// Exit code of `ensure` when no daemon could be reached.
const NO_DAEMON: i32 = 1;
/// Exit code when `log` cannot print the record.
const FAILED: i32 = 1;

/// Exit code when heli's own settings are wrong (`HELI_OUTPUT`), as for a
/// usage error.
const USAGE: i32 = 2;

/// Runs `invocation` through the daemon, or directly when it cannot; this
/// process's nesting depth is `depth`.
pub fn run(invocation: Invocation, depth: u32) -> ! {
    let child_depth = OsString::from((depth + 1).to_string());
    let locations = Locations::resolve();
    let configured = || {
        let locations = locations.as_ref().ok()?;
        Config::load(&locations.config).ok()?.output.mode
    };
    let mode = output::choose(invocation.output, configured).unwrap_or_else(|error| {
        eprintln!("cargo-heli: {error}");
        std::process::exit(USAGE)
    });
    let epoch_path = invocation.dependency_epoch.clone().or_else(|| {
        std::env::var_os("HELI_DEPENDENCY_EPOCH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let prepared = epoch_path
        .as_ref()
        .map(|path| {
            let locations = locations.as_ref().map_err(Clone::clone)?;
            dependency_cargo(&invocation)?;
            crate::dependency_home::prepare(path, &locations.state_dir.join("dependency-homes"))
        })
        .transpose()
        .unwrap_or_else(|reason| {
            eprintln!("cargo-heli: cannot prepare dependency isolation: {reason}");
            std::process::exit(USAGE);
        });
    let dependency_env = dependency_environment(prepared.as_ref()).unwrap_or_else(|reason| {
        eprintln!("cargo-heli: {reason}");
        std::process::exit(USAGE);
    });
    let reason = match locations {
        Err(error) => error,
        Ok(locations) => match reach(&locations, Some(depth)) {
            Ok(Some(stream)) => {
                let plan = output::plan(&invocation, mode);
                match submit(
                    stream,
                    &invocation,
                    &plan,
                    &child_depth,
                    prepared.as_ref(),
                    &dependency_env,
                ) {
                    Ok(never) => match never {},
                    Err(reason) => reason,
                }
            }
            Ok(None) => "no heli daemon answered".to_owned(),
            Err(reason) => reason,
        },
    };
    eprintln!("cargo-heli: warning: {reason}; running the command directly");
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .env(nesting::VAR, child_depth)
        .envs(dependency_env.iter().map(|(name, value)| (*name, value)));
    if let Some(prepared) = &prepared {
        let activity = crate::store::lease::directory().unwrap_or_else(|error| {
            eprintln!("cargo-heli: cannot protect direct execution: {error}");
            std::process::exit(USAGE);
        });
        crate::store::runner::direct(
            command,
            &[activity, prepared.epoch.lease_root().to_path_buf()],
            prepared.epoch.path(),
        );
    }
    direct::run(command)
}

/// Configuration includes at home precedence are part of this profile's
/// compatibility contract, verified with Cargo 1.99 and newer.
fn dependency_cargo(invocation: &Invocation) -> Result<(), String> {
    let cargo = if invocation.exec {
        std::env::var_os("CARGO")
            .filter(|cargo| !cargo.is_empty())
            .unwrap_or_else(|| "cargo".into())
    } else {
        invocation.program.clone()
    };
    let output = Command::new(cargo)
        .arg("--version")
        .output()
        .map_err(|error| format!("cannot identify Cargo for dependency isolation: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().nth(1).unwrap_or_default();
    let mut numbers = version.split('.');
    let major = numbers.next().and_then(|n| n.parse::<u32>().ok());
    let minor = numbers.next().and_then(|n| n.parse::<u32>().ok());
    if !output.status.success() || !matches!((major, minor), (Some(1), Some(99..)) | (Some(2..), _))
    {
        return Err("dependency isolation requires Cargo 1.99 or newer".into());
    }
    Ok(())
}

fn dependency_environment(
    prepared: Option<&crate::dependency_home::Prepared>,
) -> Result<Vec<(&'static str, OsString)>, String> {
    let Some(prepared) = prepared else {
        return Ok(Vec::new());
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    // Keep the caller's Cargo/compiler/wrapper selection. A home/bin that
    // was absent from PATH is appended only for external-tool discovery.
    let original_bin = prepared.original_home.join("bin");
    let mut entries: Vec<_> = std::env::split_paths(&path).collect();
    if !entries
        .iter()
        .any(|entry| crate::store::same_path(entry, &original_bin))
    {
        entries.push(original_bin);
    }
    let path = std::env::join_paths(entries)
        .map_err(|error| format!("cannot preserve Cargo tool search: {error}"))?;
    Ok(vec![
        ("CARGO_HOME", prepared.home.clone().into()),
        (
            "HELI_DEPENDENCY_EPOCH",
            prepared.epoch.path().as_os_str().to_owned(),
        ),
        ("PATH", path),
    ])
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

/// `cargo heli ensure`: makes sure a daemon runs, starting one unless one
/// answers; this process's nesting depth is `depth`. Silent when a daemon
/// runs, since a harness's session-start hook may show its output to the
/// agent; and quick when one already did. Exits 1, with one line saying
/// why, when none could be reached.
pub fn ensure(depth: u32) -> ! {
    let reached = Locations::resolve().and_then(|locations| reach(&locations, Some(depth)));
    match reached {
        Ok(_daemon) => std::process::exit(0),
        Err(reason) => {
            eprintln!("cargo-heli: {reason}");
            std::process::exit(NO_DAEMON)
        }
    }
}

/// Prints the part of run `run`'s record that `select` picks: stdout on
/// stdout, stderr on stderr. The daemon sends it; when none answers, this
/// client reads the record itself (and starts no daemon).
pub fn log(run: RunId, select: &Select) -> ! {
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
fn ask_log(mut stream: Stream, run: RunId, select: &Select) -> Result<Infallible, String> {
    let request = Request::Log(LogRequest {
        run: run.to_string(),
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

/// Cancels run `run`: asks the daemon, and starts none. Exits 0 once the
/// run's processes are gone, else 1 after one line saying why.
pub fn cancel(run: RunId) -> ! {
    let failure = match Locations::resolve().and_then(|locations| reach(&locations, None)) {
        Ok(None) => "no heli daemon is running, so no run is in flight".to_owned(),
        Err(reason) => format!("cannot cancel {run}: {reason}"),
        Ok(Some(mut stream)) => {
            let request = Request::Cancel(CancelRequest {
                run: run.to_string(),
            });
            let asked = protocol::write_message(&mut stream, &request)
                .and_then(|()| protocol::read_message::<Reply>(&mut stream));
            match asked {
                Ok(Reply::Cancelled) => {
                    eprintln!("cargo-heli: cancelled {run}");
                    std::process::exit(0)
                }
                Ok(Reply::CancelFailed { message }) => message,
                Ok(Reply::Unsupported) => {
                    format!("cannot cancel {run}: {}", unsupported(&mut stream))
                }
                Ok(other) => format!("cannot cancel {run}: the heli daemon answered {other:?}"),
                Err(error) => {
                    format!("cannot cancel {run}: the heli daemon did not answer: {error}")
                }
            }
        }
    };
    eprintln!("cargo-heli: {failure}");
    std::process::exit(FAILED)
}

fn query_status() -> Result<Status, String> {
    let locations = Locations::resolve()?;
    let Some(mut stream) = reach(&locations, None)? else {
        return Ok(Status::without_daemon());
    };
    let asked = protocol::write_message(&mut stream, &Request::Status)
        .and_then(|()| protocol::read_message::<Reply>(&mut stream));
    match asked {
        Ok(Reply::Status(status)) => Ok(*status),
        Ok(other) => Err(format!("the heli daemon answered {other:?} to status")),
        Err(error) => Err(format!("the heli daemon did not answer: {error}")),
    }
}

/// A connection to the daemon after a matching handshake. With `start`
/// (this client's depth), a daemon is started when none listens; without,
/// that case is `Ok(None)`. The error is the reason to fall back.
fn reach(locations: &Locations, start: Option<u32>) -> Result<Option<Stream>, String> {
    // A daemon whose endpoint is busy is there: the client waits for it as
    // long as for its answer.
    let busy_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let stream = match ipc::connect(&locations.endpoint, busy_deadline) {
        Ok(stream) => stream,
        Err(error) if ipc::is_absent(&error) => match start {
            None => return Ok(None),
            Some(depth) => start_daemon(locations, depth)?,
        },
        Err(error) if ipc::is_busy(&error) => {
            return Err(format!(
                "the heli daemon at {} took no connection within {} s",
                locations.endpoint.display(),
                HANDSHAKE_TIMEOUT.as_secs()
            ));
        }
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

/// Hands the run to the daemon, as `plan` says, and follows it to its end.
/// Returns only when the daemon did not start it, with the reason.
fn submit(
    mut stream: Stream,
    invocation: &Invocation,
    plan: &Plan,
    child_depth: &OsString,
    prepared: Option<&crate::dependency_home::Prepared>,
    dependency_env: &[(&str, OsString)],
) -> Result<std::convert::Infallible, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the working directory: {error}"))?;
    let mut overrides = vec![(nesting::VAR, child_depth.clone())];
    overrides.extend(plan.env.iter().cloned());
    overrides.extend(dependency_env.iter().cloned());
    let request = RunRequest {
        cwd: Text(cwd.into()),
        argv: std::iter::once(&invocation.program)
            .chain(&plan.args)
            .map(|arg| Text(arg.clone()))
            .collect(),
        env: environment::current_with(&overrides)
            .into_iter()
            .map(|(name, value)| (Text(name), Text(value)))
            .collect(),
        class: Some(invocation.class),
        cache: invocation.cache,
        reads_spawned: true,
        dependency_epoch: prepared
            .map(|prepared| Text(prepared.epoch.path().as_os_str().to_owned())),
    };
    stream
        .set_deadline(Some(Instant::now() + HANDSHAKE_TIMEOUT))
        .map_err(|error| format!("cannot bound the daemon's answer: {error}"))?;
    protocol::write_message(&mut stream, &Request::Run(request))
        .map_err(|error| format!("cannot hand the run to the heli daemon: {error}"))?;
    let mut reply = protocol::read_message::<Reply>(&mut stream);
    if matches!(reply, Ok(Reply::Preparing)) {
        // The daemon answered promptly; preparing execution inputs (source
        // keys or immutable epochs) is not the connection handshake deadline.
        stream
            .set_deadline(None)
            .map_err(|error| format!("cannot follow preparation: {error}"))?;
        reply = protocol::read_message::<Reply>(&mut stream);
    }
    stream
        .set_deadline(None)
        .map_err(|error| format!("cannot follow the daemon: {error}"))?;
    match reply {
        Ok(Reply::Started {
            run,
            attached,
            keyed_in,
            spawned_follows,
        }) => {
            let mut printer = plan.printer();
            let mut started = Instant::now();
            if let Some(milliseconds) = attached {
                let running_for = Duration::from_millis(milliseconds);
                started = started.checked_sub(running_for).unwrap_or(started);
                printer.note(&attached_notice(run, running_for, keyed_in.as_ref()));
            }
            // A daemon older than `Spawned` may start the command any time
            // after `Started`.
            let spawned = !spawned_follows;
            follow(stream, invocation, run, started, printer, spawned)
        }
        Ok(Reply::Passed { run, worktree, at }) => already_passed(run, &worktree, at),
        Ok(Reply::SpawnFailed { not_found, message }) => {
            direct::report_spawn_failure(&invocation.program, not_found, &message)
        }
        Ok(Reply::Refused { reason }) => Err(reason),
        Ok(Reply::Unsupported) => Err(unsupported(&mut stream)),
        Ok(other) => Err(format!("the heli daemon answered {other:?} to a run")),
        Err(error) => Err(format!("the heli daemon did not take the run: {error}")),
    }
}

/// The notice of a client attached to run `run`, going for `running_for`:
/// the same command, or with `keyed_in` the same key, of a run in that
/// worktree. Another client may follow a run of the same key, so the way
/// to run it anew is `--no-cache`, not cancelling it.
fn attached_notice(run: RunId, running_for: Duration, keyed_in: Option<&Text>) -> String {
    let age = running_for.as_secs_f64();
    match keyed_in {
        None => format!(
            "cargo-heli: attached to {run}, the same command started {age:.1} s ago; \
             to run it anew, cargo heli cancel {run} first"
        ),
        Some(worktree) => format!(
            "cargo-heli: attached to {run}, the same run on the same tree started {age:.1} s \
             ago in {}; to run it anew, give --no-cache",
            worktree.0.to_string_lossy()
        ),
    }
}

/// Prints run `run`'s output through `printer` as it arrives, then the
/// closing line, and exits as the run did; it started at `started`.
/// `spawned` says whether its command may have started already; until it
/// may (the daemon says `Spawned`), losing the daemon returns the reason to
/// run the command directly.
fn follow(
    mut stream: Stream,
    invocation: &Invocation,
    run: RunId,
    started: Instant,
    mut printer: Printer,
    mut spawned: bool,
) -> Result<Infallible, String> {
    loop {
        let failure = match protocol::read_frame::<Reply>(&mut stream) {
            Ok(Frame::Output(channel, bytes)) => {
                printer.output(channel, &bytes);
                continue;
            }
            Ok(Frame::Message(Reply::Waiting { runs })) => {
                printer.note(&format!("cargo-heli: {}", waiting_for(&runs)));
                continue;
            }
            Ok(Frame::Message(Reply::Spawned)) => {
                spawned = true;
                continue;
            }
            // A newer daemon's word about the run, which this client cannot
            // read: the run goes on all the same.
            Ok(Frame::Message(Reply::Unknown(_))) => continue,
            Ok(Frame::Message(Reply::Exited {
                code,
                signal,
                cancelled,
            })) => {
                let exit = Exit {
                    code,
                    signal,
                    cancelled,
                };
                printer.exited(&run.to_string(), exit, started.elapsed());
                exit_as(code, signal)
            }
            Ok(Frame::Message(Reply::SpawnFailed { not_found, message })) => {
                direct::report_spawn_failure(&invocation.program, not_found, &message)
            }
            Ok(Frame::Message(Reply::Passed {
                run: passed,
                worktree,
                at,
            })) => {
                drop(printer);
                already_passed(passed, &worktree, at)
            }
            Ok(Frame::Message(other)) => format!("unexpected message {other:?}"),
            Err(error) => error.to_string(),
        };
        if spawned {
            lost(run, &failure, printer)
        }
        // The command never started: running it is no second run.
        printer.abandon(&run.to_string());
        return Err(format!(
            "lost the heli daemon before {run} started: {failure}"
        ));
    }
}

/// Reports a hit of the result store: run `run` in `worktree` passed with
/// the same key at `at`. Exits 0.
fn already_passed(run: RunId, worktree: &Text, at: u64) -> ! {
    eprintln!(
        "cargo-heli: already passed: {run}, {} ({}); nothing to run",
        ago(clock::now().saturating_sub(at)),
        worktree.0.to_string_lossy()
    );
    eprintln!(
        "cargo-heli: caveats: a flaky pass is reused (--no-cache runs it again), \
         and ignored files are not part of the key"
    );
    std::process::exit(0)
}

/// `seconds` as a short age: `12 s ago`, `3 min ago`, `5 h ago`, `2 d ago`.
fn ago(seconds: u64) -> String {
    match seconds {
        0..120 => format!("{seconds} s ago"),
        120..7_200 => format!("{} min ago", seconds / 60),
        7_200..172_800 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// What the run waits for: `waiting for r17 (cargo build -p foo) to finish
/// with this worktree`, naming each run by its id and its command, the
/// program as a bare name.
fn waiting_for(runs: &[status::Run]) -> String {
    let runs: Vec<String> = runs
        .iter()
        .map(|run| {
            let mut argv = run.argv.iter();
            let program = argv.next().map(|program| {
                Path::new(program).file_stem().map_or_else(
                    || program.clone(),
                    |stem| stem.to_string_lossy().into_owned(),
                )
            });
            let command: Vec<String> = program.into_iter().chain(argv.cloned()).collect();
            format!("{} ({})", run.id, command.join(" "))
        })
        .collect();
    format!(
        "waiting for {} to finish with this worktree",
        runs.join(", ")
    )
}

fn lost(run: RunId, error: &str, printer: Printer) -> ! {
    printer.abandon(&run.to_string());
    eprintln!("cargo-heli: lost the heli daemon during run {run}: {error}");
    std::process::exit(LOST)
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
