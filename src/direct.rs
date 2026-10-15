//! Runs a command in heli's place: its stdio, environment and exit status
//! become heli's own. This is how heli runs a command when its daemon cannot.
//!
//! On Unix heli `exec`s the command, so the process is the command. Elsewhere
//! heli waits for it and exits with its code; on Windows it first ignores
//! Ctrl-C and Ctrl-Break, which the command in the same console receives too,
//! so heli reports the command's exit instead of dying before it (cargo runs
//! its own external subcommands the same way).

use std::ffi::OsStr;
use std::io;
use std::process::{Command, ExitStatus};

/// Exit code when the command does not exist (as a shell reports it).
const NOT_FOUND: i32 = 127;
/// Exit code when the command exists but cannot be started.
const NOT_STARTED: i32 = 126;

/// Runs `command` and exits with its status.
pub fn run(mut command: Command) -> ! {
    let error = replace(&mut command);
    report_spawn_failure(
        command.get_program(),
        error.kind() == io::ErrorKind::NotFound,
        &error.to_string(),
    )
}

/// Wait for a separate lifetime guardian; never replace this client with it.
/// Killing only the client must leave the guardian protecting its child.
pub fn wait(mut command: Command) -> ! {
    #[cfg(windows)]
    ignore_console_interrupts();
    let mut child = command.spawn().unwrap_or_else(|error| {
        report_spawn_failure(
            command.get_program(),
            error.kind() == io::ErrorKind::NotFound,
            &error.to_string(),
        )
    });
    loop {
        match child.wait() {
            Ok(status) => exit_as(status),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

/// Preserve all platform exit-code bits and the terminating Unix signal.
pub(crate) fn exit_as(status: ExitStatus) -> ! {
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

/// Reports that `program` could not be started, because of `error`, and
/// exits as a shell would: 127 when it does not exist, else 126.
pub fn report_spawn_failure(program: &OsStr, not_found: bool, error: &str) -> ! {
    eprintln!(
        "cargo-heli: cannot run `{}`: {error}",
        program.to_string_lossy()
    );
    std::process::exit(if not_found { NOT_FOUND } else { NOT_STARTED })
}

/// Becomes `command`; returns only the error that kept it from starting.
#[cfg(unix)]
fn replace(command: &mut Command) -> io::Error {
    use std::os::unix::process::CommandExt;

    command.exec()
}

/// Runs `command` and exits with its code; returns only the error that kept
/// it from starting.
#[cfg(not(unix))]
fn replace(command: &mut Command) -> io::Error {
    #[cfg(windows)]
    ignore_console_interrupts();
    match command.status() {
        // A Windows exit code is 32 bits and `exit` passes all of them on.
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => error,
    }
}

/// Leaves Ctrl-C and Ctrl-Break to the command. A handler (not a null one) so
/// the setting is not inherited: the command still handles them itself.
#[cfg(windows)]
fn ignore_console_interrupts() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::core::BOOL;

    unsafe extern "system" fn handled(_event: u32) -> BOOL {
        1
    }
    // SAFETY: `handled` has the signature the API requires and touches no
    // state; registering it cannot fail in a way that matters (heli then just
    // keeps the default behaviour).
    unsafe {
        SetConsoleCtrlHandler(Some(handled), 1);
    }
}
