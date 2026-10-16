//! Internal command parent keeping GC leases alive across daemon crashes.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Handshake {
    Spawned,
    SpawnFailed { not_found: bool, message: String },
}

/// Capture a discovery query without adding the normal runner's handshake
/// to its stdout. The epoch is protected before handoff and independently
/// reopened by the guardian before it starts the real consumer.
pub fn capture(command: Command, epoch: &std::path::Path) -> io::Result<std::process::Output> {
    let epoch = crate::dependencies::Epoch::open(epoch).map_err(io::Error::other)?;
    let mut guardian = Command::new(std::env::current_exe()?);
    guardian
        .arg("lease-run")
        .arg("--direct")
        .arg("--dependency-epoch")
        .arg(epoch.path())
        .arg("--root")
        .arg(epoch.lease_root())
        .arg("--")
        .arg(command.get_program())
        .args(command.get_args())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = command.get_current_dir() {
        guardian.current_dir(cwd);
    }
    // Probe commands start with env_clear and explicitly install the whole
    // request environment, including their target-directory overrides.
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            guardian.env(name, value);
        } else {
            guardian.env_remove(name);
        }
    }
    guardian.output()
}

/// Acquire before spawning: a daemon crash during startup cannot expose a
/// running command whose guardian has not yet protected its store.
pub fn main(
    roots: &[PathBuf],
    argv: &[OsString],
    epoch: Option<&std::path::Path>,
    direct: bool,
) -> ! {
    let fail = |error: io::Error| -> ! {
        if direct {
            crate::direct::report_spawn_failure(
                argv.first()
                    .map_or(std::ffi::OsStr::new("guarded command"), |arg| {
                        arg.as_os_str()
                    }),
                error.kind() == io::ErrorKind::NotFound,
                &error.to_string(),
            );
        }
        failed(error)
    };
    // Parents can both disappear before this guardian acquires its guards.
    // Reopen under ownership and recheck the snapshot before spawning, so
    // GC in that handoff window cannot launch Cargo on retired inputs.
    let _epoch = epoch
        .map(crate::dependencies::Epoch::open)
        .transpose()
        .unwrap_or_else(|error| fail(io::Error::other(error)));
    let guards = roots
        .iter()
        .map(|root| super::lease::shared(root))
        .collect::<io::Result<Vec<_>>>();
    let _guards = match guards {
        Ok(guards) => guards,
        Err(error) => fail(error),
    };
    let Some((program, args)) = argv.split_first() else {
        let error = io::Error::new(io::ErrorKind::InvalidInput, "no guarded command");
        fail(error);
    };
    let mut command = Command::new(program);
    command.args(args);
    if direct {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    protect_while_waiting(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => fail(error),
    };
    if direct {
        crate::direct::exit_as(wait(&mut child));
    }
    // This frame precedes every byte from either actual command stream.
    // Losing the daemon must not kill the command or release its leases.
    let _ = crate::protocol::write_message(&mut io::stdout().lock(), &Handshake::Spawned);
    let (done, drained) = mpsc::channel();
    for (pipe, stderr) in [
        (
            Box::new(child.stdout.take().expect("piped stdout")) as Box<dyn Read + Send>,
            false,
        ),
        (
            Box::new(child.stderr.take().expect("piped stderr")) as Box<dyn Read + Send>,
            true,
        ),
    ] {
        let done = done.clone();
        std::thread::spawn(move || {
            forward(pipe, stderr);
            let _ = done.send(());
        });
    }
    drop(done);
    // A wait error does not prove that the protected process has gone.
    let status = wait(&mut child);
    // Descendants holding pipes open do not extend the run's lifetime.
    let until = Instant::now() + Duration::from_millis(500);
    for _ in 0..2 {
        if drained
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .is_err()
        {
            break;
        }
    }
    crate::direct::exit_as(status)
}

fn failed(error: io::Error) -> ! {
    let _ = crate::protocol::write_message(
        &mut io::stdout().lock(),
        &Handshake::SpawnFailed {
            not_found: error.kind() == io::ErrorKind::NotFound,
            message: error.to_string(),
        },
    );
    std::process::exit(126)
}

/// A separate guardian survives abrupt client loss during direct fallback.
pub fn direct(command: Command, roots: &[PathBuf], epoch: &std::path::Path) -> ! {
    let executable = std::env::current_exe().unwrap_or_else(|error| {
        crate::direct::report_spawn_failure(command.get_program(), false, &error.to_string())
    });
    let mut guardian = Command::new(executable);
    guardian
        .arg("lease-run")
        .arg("--direct")
        .arg("--dependency-epoch")
        .arg(epoch);
    for root in roots {
        guardian.arg("--root").arg(root);
    }
    guardian
        .arg("--")
        .arg(command.get_program())
        .args(command.get_args());
    if let Some(cwd) = command.get_current_dir() {
        guardian.current_dir(cwd);
    }
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            guardian.env(name, value);
        } else {
            guardian.env_remove(name);
        }
    }
    crate::direct::wait(guardian)
}

fn wait(child: &mut std::process::Child) -> ExitStatus {
    loop {
        match child.wait() {
            Ok(status) => return status,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn forward(mut source: Box<dyn Read + Send>, stderr: bool) {
    let mut bytes = [0u8; 64 * 1024];
    let mut reachable = true;
    loop {
        let count = match source.read(&mut bytes) {
            Ok(0) => return,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if reachable {
            let result = if stderr {
                let mut output = io::stderr().lock();
                output
                    .write_all(&bytes[..count])
                    .and_then(|()| output.flush())
            } else {
                let mut output = io::stdout().lock();
                output
                    .write_all(&bytes[..count])
                    .and_then(|()| output.flush())
            };
            reachable = result.is_ok();
        }
    }
}

#[cfg(unix)]
fn protect_while_waiting(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn signal(number: i32, handler: usize) -> usize;
    }
    // A session cancellation reaches both guardian and child. Keep the
    // guardian alive until the actual command exits, including its TERM
    // grace period. The actual command retains the ordinary signal actions.
    // SAFETY: signal changes only this process's handlers. The pre-exec
    // callback calls only signal, which POSIX permits between fork and exec.
    unsafe {
        signal(2, 1); // SIGINT, SIG_IGN
        signal(15, 1); // SIGTERM, SIG_IGN
        command.pre_exec(|| {
            signal(2, 0); // SIG_DFL
            signal(15, 0);
            Ok(())
        });
    }
}

#[cfg(windows)]
fn protect_while_waiting(_: &mut Command) {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn handled(_: u32) -> i32 {
        1
    }
    // Custom handlers are not inherited: the child handles Ctrl-C itself.
    // A job cancellation still terminates every process in the run's job.
    // SAFETY: the callback has the console-handler ABI and no shared state.
    unsafe {
        SetConsoleCtrlHandler(Some(handled), 1);
    }
}

#[cfg(not(any(unix, windows)))]
fn protect_while_waiting(_: &mut Command) {}
