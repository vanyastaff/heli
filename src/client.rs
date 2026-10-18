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
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cli::Invocation;
use crate::config::{CargoHome, ClientConfig, Config};
use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::output::{self, Plan, Printer, print};
use crate::protocol::{
    self, CancelRequest, Channel, Frame, LogRequest, Reply, Request, RunRequest, Text,
};
use crate::run_id::RunId;
use crate::runlog::{self, select::Select};
use crate::status::{self, Exit, Status};
use crate::{class, clock, direct, environment, launch, nesting};

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
/// process's nesting depth is `depth`. Isolation is prepared before either
/// path; a daemon failure never falls back to the original mutable Cargo home.
pub fn run(mut invocation: Invocation, depth: u32) -> ! {
    let child_depth = OsString::from((depth + 1).to_string());
    let locations = Locations::resolve();
    // Isolation is a client policy, including direct fallback. A malformed
    // settings file must never silently select the shared mutable home.
    let config_path = locations
        .as_ref()
        .map(|locations| locations.config.clone())
        .or_else(|_| Locations::config_path());
    let config = match config_path {
        Ok(path) => Config::load_client(&path).unwrap_or_else(|error| {
            eprintln!("cargo-heli: {error}");
            std::process::exit(USAGE);
        }),
        Err(error)
            if std::env::var_os(crate::locations::CONFIG_VAR)
                .is_some_and(|value| !value.is_empty()) =>
        {
            eprintln!("cargo-heli: {error}");
            std::process::exit(USAGE);
        }
        // With neither a discoverable default config nor an explicit path,
        // ordinary shared-mode commands retain direct fallback.
        Err(_) => ClientConfig::default(),
    };
    let source = if invocation.exec {
        invocation.profile = Some("exec");
        crate::profiles::Source::BuiltIn
    } else {
        let source = profile(&mut invocation, &config.profiles);
        invocation.profile = Some(source.name(&invocation.args));
        source
    };
    let configured = || config.output.mode;
    let mode = output::choose(invocation.output, configured).unwrap_or_else(|error| {
        eprintln!("cargo-heli: {error}");
        std::process::exit(USAGE)
    });
    let epoch_path = invocation.dependency_epoch.clone().or_else(|| {
        std::env::var_os("HELI_DEPENDENCY_EPOCH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let inherited = crate::dependency_home::inherited().unwrap_or_else(|error| {
        eprintln!("cargo-heli: cannot inspect inherited Cargo home: {error}");
        std::process::exit(USAGE);
    });
    let isolated = isolated_home(
        invocation.cargo_home,
        config.cargo.home,
        epoch_path.is_some(),
        inherited,
    )
    .unwrap_or_else(|error| {
        eprintln!("cargo-heli: {error}");
        std::process::exit(USAGE);
    });
    // A command that only prints runs as bare Cargo would, so `install --list`
    // lists the caller's home. Inside an isolated parent the home is already
    // the private one and stays so.
    let isolated = isolated
        && (epoch_path.is_some()
            || inherited
            || !cargo_arguments(&invocation).is_some_and(reads_home_only));
    let prepared = isolated
        .then(|| {
            let locations = locations.as_ref().map_err(Clone::clone)?;
            isolated_command(&invocation)?;
            dependency_cargo(&invocation)?;
            crate::dependency_home::prepare(
                epoch_path.as_deref(),
                &locations.state_dir.join(if epoch_path.is_some() {
                    "dependency-homes"
                } else {
                    "h"
                }),
            )
        })
        .transpose()
        .unwrap_or_else(|reason| {
            eprintln!("cargo-heli: cannot prepare isolated Cargo home: {reason}");
            std::process::exit(USAGE);
        });
    let dependency_env = dependency_environment(prepared.as_ref()).unwrap_or_else(|reason| {
        eprintln!("cargo-heli: {reason}");
        std::process::exit(USAGE);
    });
    // Held until this process ends, with the run: heli's own eviction and a
    // tool that deletes build output never delete at the same time.
    let _cleaning = (!invocation.exec)
        .then(|| class::cleaner_note(&invocation.args))
        .flatten()
        .and_then(|note| clean_alongside(note, locations.as_ref().ok()));
    // A tool a profile names is no unknown tool.
    let in_pool = !invocation.exec
        && source == crate::profiles::Source::BuiltIn
        && class::runs_directly(&invocation.args, std::io::stdin().is_terminal());
    let route = match locations {
        Err(error) => Route::Fallback { reason: error },
        Ok(locations) => match reach(&locations, Some(depth)) {
            // An interactive or unknown tool runs here, with the host pool.
            Ok(Some(stream)) if in_pool => {
                drop(stream);
                record_direct(&locations, &invocation);
                match crate::pool::makeflags(&locations.pool) {
                    Ok(makeflags) => Route::InPool { makeflags },
                    Err(error) => Route::Fallback {
                        reason: error.to_string(),
                    },
                }
            }
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
                    Err(reason) => Route::Fallback { reason },
                }
            }
            Ok(None) => Route::Fallback {
                reason: "no heli daemon answered".to_owned(),
            },
            Err(reason) => Route::Fallback { reason },
        },
    };
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .env(nesting::VAR, child_depth)
        .envs(dependency_env.iter().map(|(name, value)| (*name, value)));
    match &route {
        Route::InPool { makeflags } => {
            command.env("CARGO_MAKEFLAGS", makeflags);
        }
        Route::Fallback { reason } => {
            eprintln!("cargo-heli: warning: {reason}; running the command directly");
        }
    }
    if let Some(prepared) = &prepared {
        let activity = crate::store::lease::directory().unwrap_or_else(|error| {
            eprintln!("cargo-heli: cannot protect direct execution: {error}");
            std::process::exit(USAGE);
        });
        let mut roots = vec![activity];
        if let Some(epoch) = &prepared.epoch {
            roots.push(epoch.lease_root().to_path_buf());
        }
        crate::store::runner::direct(
            command,
            &roots,
            prepared.epoch.as_ref().map(|epoch| epoch.path()),
        );
    }
    direct::run(command)
}

/// Says `note` before a tool that deletes build output runs, and takes
/// heli's eviction lock (`crate::store::evict::lock_file`) for the run:
/// the daemon's eviction passes skip while it is held, `cargo heli gc`
/// waits, and the tool waits for a pass in progress. Without the lock (no
/// state directory) the tool still runs, with a warning.
fn clean_alongside(note: &str, locations: Option<&Locations>) -> Option<std::fs::File> {
    eprintln!("cargo-heli: note: {note}");
    let locations = locations?;
    let opened = locations.create_state_dir().and_then(|()| {
        std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(crate::store::evict::lock_file(&locations.state_dir))
    });
    let locked = opened.and_then(|file| match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            eprintln!("cargo-heli: waiting for heli's eviction pass to end");
            file.lock().map(|()| file)
        }
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    });
    match locked {
        Ok(file) => Some(file),
        Err(error) => {
            eprintln!(
                "cargo-heli: warning: cannot take heli's eviction lock ({error}); running alongside it"
            );
            None
        }
    }
}

/// How the client runs a command itself, once the daemon does not run it
/// (a run the daemon takes never returns to the client's own code).
enum Route {
    /// Drawing from the host pool, as an interactive or unknown tool does
    /// (`crate::class::runs_directly`): its `CARGO_MAKEFLAGS`.
    InPool { makeflags: String },
    /// Without the daemon or its pool, for `reason`, which the client warns
    /// of.
    Fallback { reason: String },
}

/// Logs a run the client makes itself in the pool (`crate::class::
/// runs_directly`) in the daemon's event log, as `direct`, with its
/// `worktree`, `argv` and `profile`, which `cargo heli doctor` counts.
fn record_direct(locations: &Locations, invocation: &Invocation) {
    let worktree = std::env::current_dir()
        .map(|cwd| crate::worktree::of(&cwd))
        .unwrap_or_default();
    let argv: Vec<String> = std::iter::once(&invocation.program)
        .chain(&invocation.args)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    crate::daemon::events::Events::new(locations.event_log(), crate::daemon::events::DEFAULT_MAX)
        .record(
            "direct",
            serde_json::json!({
                "worktree": worktree.to_string_lossy(),
                "argv": argv,
                "profile": invocation.profile,
            }),
        );
}

/// Gives a cargo subcommand's run the class its tool profile says
/// (`crate::profiles`): the user's `profiles`, and the repository's
/// `.config/heli.toml` in the worktree here. Returns where the class came
/// from.
fn profile(
    invocation: &mut Invocation,
    profiles: &crate::profiles::Profiles,
) -> crate::profiles::Source {
    let worktree = std::env::current_dir()
        .map(|cwd| crate::worktree::of(&cwd))
        .unwrap_or_default();
    let repository = crate::profiles::repository(&worktree).unwrap_or_else(|reason| {
        eprintln!("cargo-heli: warning: {reason}; ignoring the repository's tool profiles");
        Vec::new()
    });
    let (class, source) = crate::profiles::resolve(&invocation.args, profiles, &repository);
    invocation.reclassify(class);
    source
}

fn isolated_home(
    explicit: Option<CargoHome>,
    configured: CargoHome,
    epoch: bool,
    inherited: bool,
) -> Result<bool, String> {
    if explicit == Some(CargoHome::Shared) && (epoch || inherited) {
        return Err("an explicit shared Cargo home conflicts with an epoch or inherited isolated home; select shared mode at the outer command boundary".into());
    }
    Ok(epoch || inherited || explicit.unwrap_or(configured) == CargoHome::Isolated)
}

fn cargo_arguments(invocation: &Invocation) -> Option<&[OsString]> {
    (!invocation.exec
        || Path::new(&invocation.program)
            .file_stem()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("cargo")))
    .then_some(invocation.args.as_slice())
}

fn isolated_command(invocation: &Invocation) -> Result<(), String> {
    let Some(args) = cargo_arguments(invocation) else {
        return Ok(());
    };
    let Some(command) = isolated_subcommand(args)? else {
        return Ok(());
    };
    if !reads_home_only(args)
        && (["install", "uninstall", "login", "logout", "publish"]
            .iter()
            .any(|name| command == *name)
            || command.to_str().is_some_and(class::writes_home))
    {
        return Err(format!(
            "cargo {} is not supported with an isolated Cargo home; use bare Cargo with the original home or --cargo-home shared at the outer command boundary",
            command.to_string_lossy()
        ));
    }
    Ok(())
}

/// Whether Cargo's arguments only print: `install --list`, which reads the
/// home's record of installed tools, or the help or version of a command. Such
/// a command never writes the Cargo home, and `install --list` is only right
/// when it reads the caller's own home. `install foo --help` counts as help; a
/// `--version` or `-V` counts only as the sole argument, since `install` and
/// `binstall` take `--version <semver>` to pick a crate version.
fn reads_home_only(args: &[OsString]) -> bool {
    let Ok(Some(command)) = isolated_subcommand(args) else {
        return false;
    };
    let rest: Vec<&OsString> = args
        .iter()
        .skip_while(|arg| arg.as_os_str() != command)
        .skip(1)
        .take_while(|arg| *arg != "--")
        .collect();
    let has = |flags: &[&str]| rest.iter().any(|arg| flags.iter().any(|flag| *arg == flag));
    (command == "install" && has(&["--list"]))
        || has(&["-h", "--help"])
        || (rest.len() == 1 && has(&["-V", "--version"]))
}

/// Read only Cargo's leading global options for the isolation guard. Keep the
/// original argv untouched; an unrecognized option cannot hide a home mutation.
fn isolated_subcommand(mut args: &[OsString]) -> Result<Option<&OsStr>, String> {
    if args
        .first()
        .is_some_and(|arg| arg.as_encoded_bytes().starts_with(b"+"))
    {
        args = &args[1..];
    }
    while let Some((arg, rest)) = args.split_first() {
        let bytes = arg.as_encoded_bytes();
        if !bytes.starts_with(b"-") {
            return Ok(Some(arg));
        }
        let text = arg.to_str().unwrap_or_default();
        if matches!(text, "--color" | "--config" | "-Z" | "-C") {
            let Some((_, remaining)) = rest.split_first() else {
                return Err(format!(
                    "Cargo option {text} requires a value in isolated mode"
                ));
            };
            args = remaining;
        } else if matches!(
            text,
            "--verbose"
                | "--quiet"
                | "--locked"
                | "--offline"
                | "--frozen"
                | "--help"
                | "--version"
                | "--list"
                | "-q"
                | "-h"
                | "-V"
        ) || text.starts_with("--color=")
            || text.starts_with("--config=")
            || (bytes.len() > 2 && (bytes.starts_with(b"-Z") || bytes.starts_with(b"-C")))
            || (bytes.len() > 1 && bytes[0] == b'-' && bytes[1..].iter().all(|b| *b == b'v'))
        {
            args = rest;
        } else {
            return Err(format!(
                "cannot identify the Cargo command after option {} in isolated mode; use a supported global option or bare Cargo with the original home",
                arg.to_string_lossy()
            ));
        }
    }
    Ok(None)
}

/// Configuration includes at home precedence are part of this profile's
/// compatibility contract, verified with Cargo 1.99 and newer.
fn dependency_cargo(invocation: &Invocation) -> Result<(), String> {
    let cargo = if cargo_arguments(invocation).is_none() {
        std::env::var_os("CARGO")
            .filter(|cargo| !cargo.is_empty())
            .unwrap_or_else(|| "cargo".into())
    } else {
        invocation.program.clone()
    };
    let mut probe = Command::new(cargo);
    if let Some(selector) = cargo_arguments(invocation)
        .and_then(|args| args.first())
        .filter(|arg| arg.as_encoded_bytes().starts_with(b"+"))
    {
        probe.arg(selector);
    }
    let output = probe
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
        return Err("isolated Cargo homes require Cargo 1.99 or newer".into());
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
    let mut overrides = vec![("CARGO_HOME", prepared.home.clone().into()), ("PATH", path)];
    if let Some(epoch) = &prepared.epoch {
        overrides.push(("HELI_DEPENDENCY_EPOCH", epoch.path().as_os_str().to_owned()));
    }
    Ok(overrides)
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
    let reached = Locations::resolve().and_then(|locations| {
        warn_of_package_redirection(&locations);
        reach(&locations, Some(depth))
    });
    match reached {
        Ok(_daemon) => std::process::exit(0),
        Err(reason) => {
            eprintln!("cargo-heli: {reason}");
            std::process::exit(NO_DAEMON)
        }
    }
}

/// Warns when Windows package virtualization redirects heli's state or
/// configuration (`crate::locations::package_redirection`): this process
/// then sees files no process outside its app sees. `cargo heli doctor`
/// says what to do.
fn warn_of_package_redirection(locations: &Locations) {
    for (what, path) in [
        ("state directory", &locations.state_dir),
        ("configuration", &locations.config),
    ] {
        if let Some(actual) = crate::locations::package_redirection(path) {
            eprintln!(
                "cargo-heli: warning: Windows redirects heli's {what} {} to {} for this app only; processes outside it use another (see `cargo heli doctor`)",
                path.display(),
                actual.display()
            );
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

/// `cargo heli stop`, and with `restart` (this process's nesting depth)
/// `cargo heli restart`: asks the daemon to retire, cancelling the runs in
/// flight first with `now` (after a confirmation unless `yes`). A restart
/// then waits until that daemon has exited and starts a new one, through
/// the autostart registration when it names this program (`crate::launch`).
/// Exits 1, with one line saying why, when that fails.
pub fn stop(now: bool, yes: bool, restart: Option<u32>) -> ! {
    match stop_daemon(now, yes, restart) {
        Ok(()) => std::process::exit(0),
        Err(reason) => {
            eprintln!("cargo-heli: {reason}");
            std::process::exit(FAILED)
        }
    }
}

fn stop_daemon(now: bool, yes: bool, restart: Option<u32>) -> Result<(), String> {
    let locations = Locations::resolve()?;
    let status = query_status_at(&locations)?;
    let Some(daemon) = &status.daemon else {
        eprintln!("cargo-heli: no heli daemon is running");
        if let Some(depth) = restart {
            return start_reporting(&locations, depth);
        }
        return Ok(());
    };
    let pid = daemon.pid;
    let in_flight: Vec<&str> = status
        .runs
        .iter()
        .filter(|run| run.exit.is_none())
        .map(|run| run.id.as_str())
        .collect();
    if now && !in_flight.is_empty() && !yes {
        eprint!(
            "Cancel {} run(s) in flight ({})? [y/N] ",
            in_flight.len(),
            in_flight.join(", ")
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        let _ = io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            return Err("nothing stopped".to_owned());
        }
    }
    // Cancelled first: a retired daemon exits as its last run ends, before
    // it could answer the cancel.
    if now {
        for run in &in_flight {
            if let Some(mut stream) = reach(&locations, None)? {
                let request = Request::Cancel(CancelRequest {
                    run: (*run).to_owned(),
                });
                match protocol::write_message(&mut stream, &request)
                    .and_then(|()| protocol::read_message::<Reply>(&mut stream))
                {
                    Ok(Reply::Cancelled) => eprintln!("cargo-heli: cancelled {run}"),
                    Ok(Reply::CancelFailed { message }) => {
                        eprintln!("cargo-heli: warning: {message}");
                    }
                    _ => eprintln!("cargo-heli: warning: cannot cancel {run}"),
                }
            }
        }
    }
    let Some(mut stream) = reach(&locations, None)? else {
        eprintln!("cargo-heli: the heli daemon (pid {pid}) has exited");
        return match restart {
            Some(depth) => start_reporting(&locations, depth),
            None => Ok(()),
        };
    };
    let asked = protocol::write_message(&mut stream, &Request::Stop)
        .and_then(|()| protocol::read_message::<Reply>(&mut stream));
    let runs = match asked {
        Ok(Reply::Stopping { runs }) => Some(runs),
        // Older than `stop`: it retires as after a version mismatch.
        Ok(Reply::Unsupported) => {
            let _ = unsupported(&mut stream);
            None
        }
        Ok(other) => return Err(format!("the heli daemon answered {other:?} to stop")),
        Err(error) => return Err(format!("the heli daemon did not answer: {error}")),
    };
    drop(stream);
    match runs {
        Some(0) => eprintln!("cargo-heli: the heli daemon (pid {pid}) exits now"),
        _ if now => eprintln!("cargo-heli: the heli daemon (pid {pid}) exits now"),
        Some(runs) => eprintln!(
            "cargo-heli: the heli daemon (pid {pid}) takes no new runs and exits once its {runs} run(s) in flight end"
        ),
        None => eprintln!(
            "cargo-heli: the heli daemon (pid {pid}) is older than this cargo-heli; it takes no new runs and exits once idle"
        ),
    }
    let Some(depth) = restart else {
        return Ok(());
    };
    let mut told = false;
    loop {
        match query_status_at(&locations).map(|status| status.daemon) {
            // Still there, or busy exiting.
            Ok(Some(daemon)) if daemon.pid == pid => {}
            Err(_) => {}
            // Gone; or a client has started a new daemon already.
            Ok(_) => break,
        }
        if !told {
            eprintln!("cargo-heli: waiting for it to exit");
            told = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    start_reporting(&locations, depth)
}

/// Makes sure a daemon runs, as `ensure` does, and says which.
fn start_reporting(locations: &Locations, depth: u32) -> Result<(), String> {
    reach(locations, Some(depth))?;
    match query_status_at(locations)?.daemon {
        Some(daemon) => {
            eprintln!(
                "cargo-heli: the heli daemon runs: pid {}, version {}",
                daemon.pid,
                daemon.version.as_deref().unwrap_or("before 0.0.3")
            );
            Ok(())
        }
        None => Err("the new heli daemon did not answer".to_owned()),
    }
}

fn query_status() -> Result<Status, String> {
    query_status_at(&Locations::resolve()?)
}

fn query_status_at(locations: &Locations) -> Result<Status, String> {
    let Some(mut stream) = reach(locations, None)? else {
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
        profile: invocation.profile.map(str::to_owned),
        dependency_epoch: prepared
            .and_then(|prepared| prepared.epoch.as_ref())
            .map(|epoch| Text(epoch.path().as_os_str().to_owned())),
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

#[cfg(test)]
mod isolation_options {
    use super::isolated_subcommand;
    use std::ffi::{OsStr, OsString};

    #[test]
    fn global_options_cannot_hide_a_home_command() {
        for prefix in [
            vec!["+nightly", "--color", "never"],
            vec!["--color=never", "--config", "net.retry=2"],
            vec!["--config=net.retry=2", "-Z", "unstable-options"],
            vec!["-Zunstable-options", "-C", "elsewhere"],
            vec!["-Celsewhere", "-vv", "--locked", "--offline", "--frozen"],
        ] {
            let args: Vec<OsString> = prefix
                .into_iter()
                .chain(["install", "--list"])
                .map(Into::into)
                .collect();
            assert_eq!(
                isolated_subcommand(&args).unwrap(),
                Some(OsStr::new("install"))
            );
        }
    }

    #[test]
    fn unknown_or_incomplete_global_options_refuse_isolation() {
        for args in [
            vec!["--unknown", "login"],
            vec!["--color"],
            vec!["-Z"],
            vec!["--", "install"],
        ] {
            let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
            assert!(isolated_subcommand(&args).is_err());
        }
    }
}
