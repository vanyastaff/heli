//! Starting the daemon in the background, from a client.
//!
//! The daemon is `cargo-heli daemon`, started from the client's own binary.
//! It stays in the client's logon session (on Windows, its window station
//! and desktop too), so the runs it starts can reach the user's desktop; it
//! is never a service. What it must not keep from the client:
//!
//! - **Its place in the client's process tree.** Agent harnesses stop a
//!   command by killing its process tree, found through parent processes
//!   (observed with Claude Code on Windows: stopping a command killed the
//!   client, the daemon it had started and that daemon's run). So the client
//!   does not start the daemon itself: it starts `cargo-heli daemon
//!   --detach`, which starts the daemon, waits until it listens (or relays
//!   the exit code with which it failed), and exits. The daemon's parent is
//!   then gone, and no tree walk from the client reaches it.
//! - **Handles.** A daemon that inherited the client's stdout or stderr
//!   would keep the pipe of whoever reads the client (an agent's tool call)
//!   open until the daemon exits. On Windows it is started with
//!   `CreateProcessW` and no inheritance at all (the standard library always
//!   lets a child inherit every inheritable handle); on Unix its stdio is
//!   `/dev/null` and the standard library opens everything else
//!   close-on-exec.
//! - **The client's terminal signals and job.** On Unix it leads a process
//!   group of its own, so Ctrl-C in the client's terminal does not reach it.
//!   On Windows it gets a console of its own without a window
//!   (`CREATE_NO_WINDOW`), which its runs share, and leaves the client's job
//!   object when the job allows breakaway. Claude Code's job does not: the
//!   daemon then stays in that job, which holds the whole app and is never
//!   killed as a whole in normal operation (no kill-on-close limit). A job
//!   that is terminated as a whole still takes the daemon with it; only a
//!   daemon started outside the job avoids that. So once `setup
//!   --autostart` has registered the logon task, the client does not start
//!   the daemon itself: it asks Task Scheduler to (`schtasks /Run`), and
//!   likewise the systemd user manager or launchd elsewhere
//!   (`crate::autostart`). It starts the daemon itself only when there is
//!   no registration for its program and locations, or the service manager
//!   refuses.
//! - **The working directory**, which would keep the client's worktree busy
//!   on Windows: it runs in the state directory.
//!
//! Each stage gets its parent's environment, plus the resolved locations,
//! and `HELI_DEPTH` one deeper, like any command heli starts.

use std::ffi::OsString;
use std::io;
use std::time::{Duration, Instant};

use crate::ipc::{self, Stream};
use crate::locations::Locations;
use crate::{autostart, daemon, environment, nesting};

/// How long a starting daemon may take to listen.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Exit code of `daemon --detach` when no daemon listened within
/// [`START_TIMEOUT`]. The client waits with the same deadline, and which of
/// the two expires first is a race; the client reads this code as
/// [`StartError::TimedOut`], so it reports a timeout either way.
const DETACH_TIMED_OUT: i32 = 4;

/// A start this process launched: the stage it started, or, when the
/// autostart registration was asked to start the daemon, nothing it can
/// watch.
pub struct Launched {
    #[cfg(unix)]
    child: Option<std::process::Child>,
    #[cfg(windows)]
    process: Option<std::os::windows::io::OwnedHandle>,
}

/// Why no daemon listens.
pub enum StartError {
    /// The process exited with this code before a daemon listened.
    Exited(i32),
    /// Nothing listened within [`START_TIMEOUT`].
    TimedOut,
    /// Something is at the endpoint, but it cannot be used.
    Unreachable(io::Error),
}

/// Starts a daemon for `locations`, outside this process's tree; `depth` is
/// this client's. When setup registered the daemon to start at logon for
/// this program and these locations, the registration starts it
/// (`crate::autostart`); when there is none, or the service manager
/// refuses, this process does. Follow with [`wait`].
pub fn start(locations: &Locations, depth: u32) -> io::Result<Launched> {
    if autostart::start(locations, depth) {
        return Ok(Launched {
            #[cfg(unix)]
            child: None,
            #[cfg(windows)]
            process: None,
        });
    }
    spawn_stage(locations, depth, &["daemon", "--detach"])
}

/// `cargo-heli daemon --detach`: starts the daemon, exits 0 once it listens,
/// [`DETACH_TIMED_OUT`] when nothing listened in time, or with the code the
/// daemon exited with, or 1. It creates the state directory, where the
/// daemon runs, since the logon task may run it before anything else has.
pub fn detach(depth: u32) -> ! {
    let Ok(locations) = Locations::resolve() else {
        std::process::exit(1)
    };
    if locations.create_state_dir().is_err() {
        std::process::exit(1)
    }
    let Ok(mut daemon) = spawn_stage(&locations, depth, &["daemon"]) else {
        std::process::exit(1)
    };
    std::process::exit(match wait(&locations, &mut daemon) {
        Ok(_probe) => 0,
        Err(StartError::Exited(code)) => code,
        Err(StartError::TimedOut) => DETACH_TIMED_OUT,
        Err(StartError::Unreachable(_)) => 1,
    })
}

/// Waits until a daemon listens at the endpoint, while `launched` (the
/// detaching stage, or the daemon itself) has not failed, and connects.
/// Exiting with 0 or [`daemon::ALREADY_RUNNING`] is no failure: a daemon
/// listens, or will. A daemon the autostart registration starts is not
/// this process's to watch, so only the deadline ends that wait.
pub fn wait(locations: &Locations, launched: &mut Launched) -> Result<Stream, StartError> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match ipc::connect(&locations.endpoint) {
            Ok(stream) => return Ok(stream),
            Err(error) if ipc::is_absent(&error) => {}
            Err(error) => return Err(StartError::Unreachable(error)),
        }
        match launched.exit_code() {
            None | Some(0 | daemon::ALREADY_RUNNING) => {}
            Some(DETACH_TIMED_OUT) => return Err(StartError::TimedOut),
            Some(code) => return Err(StartError::Exited(code)),
        }
        if Instant::now() >= deadline {
            return Err(StartError::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl StartError {
    /// Why no daemon could be reached, for a warning.
    pub fn describe(&self, locations: &Locations) -> String {
        match self {
            Self::Exited(code) => format!(
                "the heli daemon exited with code {code} as it started (see {})",
                locations.log_file().display()
            ),
            Self::TimedOut => format!(
                "the heli daemon did not listen within {} s of its start",
                START_TIMEOUT.as_secs()
            ),
            Self::Unreachable(error) => format!(
                "cannot reach the heli daemon at {}: {error}",
                locations.endpoint.display()
            ),
        }
    }
}

/// Starts `cargo-heli <args>` detached from this process.
fn spawn_stage(locations: &Locations, depth: u32, args: &[&str]) -> io::Result<Launched> {
    let program = std::env::current_exe()?;
    let mut overrides: Vec<(&str, OsString)> = locations
        .vars()
        .into_iter()
        .map(|(name, value)| (name, value.to_os_string()))
        .collect();
    overrides.push((nesting::VAR, OsString::from((depth + 1).to_string())));
    let env = environment::current_with(&overrides);
    spawn(&program, args, locations, env)
}

impl Launched {
    /// The exit code once the process has exited; `None` while it runs, or
    /// when there is no process to watch.
    #[cfg(unix)]
    pub fn exit_code(&mut self) -> Option<i32> {
        self.child
            .as_mut()?
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.code().unwrap_or(1))
    }

    /// The exit code once the process has exited; `None` while it runs, or
    /// when there is no process to watch.
    #[cfg(windows)]
    pub fn exit_code(&mut self) -> Option<i32> {
        windows::exit_code(self.process.as_ref()?)
    }
}

#[cfg(unix)]
fn spawn(
    program: &std::path::Path,
    args: &[&str],
    locations: &Locations,
    env: Vec<(OsString, OsString)>,
) -> io::Result<Launched> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(&locations.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(Launched { child: Some(child) })
}

#[cfg(windows)]
fn spawn(
    program: &std::path::Path,
    args: &[&str],
    locations: &Locations,
    env: Vec<(OsString, OsString)>,
) -> io::Result<Launched> {
    let process = windows::create_process(program, args, &locations.state_dir, env)?;
    Ok(Launched {
        process: Some(process),
    })
}

#[cfg(windows)]
mod windows {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
    };

    /// Starts `"<program>" <args>` in `dir` with exactly `env`, inheriting
    /// no handle; `args` need no quoting.
    pub fn create_process(
        program: &Path,
        args: &[&str],
        dir: &Path,
        mut env: Vec<(OsString, OsString)>,
    ) -> io::Result<OwnedHandle> {
        let application = wide(program.as_os_str());
        // A path cannot contain `"`, so quoting it is enough.
        let mut command_line: Vec<u16> = OsStr::new("\"")
            .encode_wide()
            .chain(program.as_os_str().encode_wide())
            .chain(OsStr::new("\"").encode_wide())
            .chain(args.iter().flat_map(|arg| {
                OsStr::new(" ")
                    .encode_wide()
                    .chain(OsStr::new(arg).encode_wide())
            }))
            .chain([0])
            .collect();
        // Windows expects the block sorted by name, ignoring case.
        env.sort_by_cached_key(|(name, _)| name.to_ascii_uppercase());
        let mut block: Vec<u16> = Vec::new();
        for (name, value) in &env {
            block.extend(name.encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        let dir = wide(dir.as_os_str());
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };

        let mut breakaway = CREATE_BREAKAWAY_FROM_JOB;
        loop {
            let mut info = PROCESS_INFORMATION::default();
            // SAFETY: every string is NUL-terminated, the environment block
            // double-NUL-terminated, and all outlive the call.
            let ok = unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command_line.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | breakaway,
                    block.as_ptr().cast(),
                    dir.as_ptr(),
                    &startup,
                    &mut info,
                )
            };
            if ok != 0 {
                // SAFETY: handles returned by CreateProcessW, owned here.
                unsafe {
                    CloseHandle(info.hThread);
                    return Ok(OwnedHandle::from_raw_handle(info.hProcess));
                }
            }
            let error = io::Error::last_os_error();
            // A job that does not allow breakaway refuses the flag.
            if breakaway != 0 && error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                breakaway = 0;
                continue;
            }
            return Err(error);
        }
    }

    pub fn exit_code(process: &OwnedHandle) -> Option<i32> {
        let mut code = 0;
        // SAFETY: a process handle with query access (CreateProcessW grants
        // all access).
        let ok = unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) };
        (ok != 0 && code != STILL_ACTIVE as u32).then_some(code as i32)
    }

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain([0]).collect()
    }
}
