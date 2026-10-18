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
//! stdout open, so the daemon takes what is left in the pipes until they
//! have been quiet for a short grace period (output still arriving restarts
//! it, up to a limit), then reports the exit and keeps draining the pipes
//! into the void until they close. A grace period counted from the exit
//! alone would cut off the end of a large output that takes longer than it
//! to read.

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

/// How long the pipes may stay quiet, after the run's process has exited,
/// before the run is over: output still arriving restarts it.
const GRACE: Duration = Duration::from_millis(500);

/// How long after the run's process has exited its output is taken at most,
/// however steadily processes it left behind keep writing.
const DRAIN_LIMIT: Duration = Duration::from_secs(10);

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
    let Some(wrapper) = wrappers(request, cwd).into_iter().find(|wrapper| {
        wrapper
            .file_stem()
            .is_some_and(|stem| stem.eq_ignore_ascii_case("sccache"))
    }) else {
        return;
    };
    let mut command = Command::new(wrapper);
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

/// The compiler wrappers cargo uses for `request`: `rustc-wrapper` and
/// `rustc-workspace-wrapper`, each from its variable (`RUSTC_WRAPPER`, else
/// `CARGO_BUILD_RUSTC_WRAPPER`, and the same for the workspace wrapper), else
/// from the nearest Cargo configuration that sets it under `[build]`: the
/// `.cargo/` of the working directory and every directory above it, then the
/// Cargo home. A path there with a separator is relative to the directory
/// holding that `.cargo/` (cargo's rule); a bare name is left to the search
/// path. A variable set empty turns the wrapper off.
fn wrappers(request: &RunRequest, cwd: &Path) -> Vec<PathBuf> {
    let homes = cwd
        .ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(super::key::cargo_home(request, cwd));
    let mut configs = Vec::new();
    for dir in homes {
        for file in ["config.toml", "config"] {
            if let Ok(text) = std::fs::read_to_string(dir.join(file))
                && let Ok(table) = text.parse::<toml::Table>()
            {
                configs.push((dir.clone(), table));
                break;
            }
        }
    }
    let mut found = Vec::new();
    for (key, vars) in [
        (
            "rustc-wrapper",
            ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"],
        ),
        (
            "rustc-workspace-wrapper",
            [
                "RUSTC_WORKSPACE_WRAPPER",
                "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
            ],
        ),
    ] {
        if let Some(value) = vars.iter().find_map(|var| request.var(var)) {
            if !value.is_empty() {
                found.push(resolve(cwd, value.to_owned()));
            }
            continue;
        }
        let configured = configs.iter().find_map(|(dir, table)| {
            let value = table.get("build")?.as_table()?.get(key)?.as_str()?;
            let path = PathBuf::from(value);
            Some(if path.is_relative() && path.components().count() > 1 {
                dir.parent().unwrap_or(dir).join(path)
            } else {
                path
            })
        });
        found.extend(configured);
    }
    found
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
    let mut limit: Option<Instant> = None;
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
                if let Some(limit) = limit {
                    deadline = Some((Instant::now() + GRACE).min(limit));
                }
            }
            Some(Event::Closed) => open -= 1,
            Some(Event::Exited(status)) => {
                exit = Some(status);
                let now = Instant::now();
                limit = Some(now + DRAIN_LIMIT);
                deadline = Some(now + GRACE);
            }
            // The pipes were quiet for the grace period, or the limit is
            // reached: processes the run left behind hold them.
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::wrappers;
    use crate::protocol::{RunRequest, Text};

    fn request(cwd: &Path, env: &[(&str, &str)], home: &Path) -> RunRequest {
        let mut env: Vec<(Text, Text)> = env
            .iter()
            .map(|(name, value)| (Text(OsString::from(name)), Text(OsString::from(value))))
            .collect();
        env.push((Text("CARGO_HOME".into()), Text(home.as_os_str().to_owned())));
        RunRequest {
            cwd: Text(cwd.as_os_str().to_owned()),
            argv: vec![Text("cargo".into()), Text("build".into())],
            env,
            dependency_epoch: None,
            class: None,
            cache: Default::default(),
            reads_spawned: false,
            profile: None,
            priority: Default::default(),
            invocation: None,
            rerun_of: None,
        }
    }

    #[test]
    fn wrappers_come_from_variables_then_the_nearest_configuration() {
        let root = tempfile::tempdir().expect("dir");
        let home = root.path().join("home");
        let work = root.path().join("ws").join("crate");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(work.join(".cargo")).expect("work");
        std::fs::write(
            home.join("config.toml"),
            "[build]\nrustc-wrapper = \"sccache\"\n",
        )
        .expect("home config");
        // The Cargo home's configuration applies when nothing nearer does.
        assert_eq!(
            wrappers(&request(&work, &[], &home), &work),
            [Path::new("sccache")]
        );
        std::fs::write(
            work.join(".cargo").join("config.toml"),
            "[build]\nrustc-wrapper = \"tools/sccache\"\nrustc-workspace-wrapper = \"clippy-ish\"\n",
        )
        .expect("config");
        assert_eq!(
            wrappers(&request(&work, &[], &home), &work),
            [work.join("tools").join("sccache"), "clippy-ish".into()]
        );
        // A variable wins; set empty, it turns the wrapper off.
        let set = request(&work, &[("RUSTC_WRAPPER", "my-wrapper")], &home);
        assert_eq!(wrappers(&set, &work)[0], Path::new("my-wrapper"));
        let off = request(&work, &[("RUSTC_WRAPPER", "")], &home);
        assert_eq!(wrappers(&off, &work), [Path::new("clippy-ish")]);
    }
}
