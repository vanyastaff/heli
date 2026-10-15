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

/// Acquire before spawning: a daemon crash during startup cannot expose a
/// running command whose guardian has not yet protected its store.
pub fn main(roots: &[PathBuf], argv: &[OsString]) -> ! {
    let guards = roots
        .iter()
        .map(|root| super::lease::shared(root))
        .collect::<io::Result<Vec<_>>>();
    let _guards = match guards {
        Ok(guards) => guards,
        Err(error) => failed(error),
    };
    let Some((program, args)) = argv.split_first() else {
        failed(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no guarded command",
        ));
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    protect_while_waiting(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => failed(error),
    };
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
    let status = loop {
        match child.wait() {
            Ok(status) => break status,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
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
    exit_as(status)
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

fn exit_as(status: ExitStatus) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(number) = status.signal() {
            unsafe extern "C" {
                fn signal(number: i32, handler: usize) -> usize;
                fn raise(number: i32) -> i32;
            }
            // SAFETY: restore and raise the child's terminating signal.
            unsafe {
                signal(number, 0);
                raise(number);
            }
        }
    }
    std::process::exit(status.code().unwrap_or(1))
}
