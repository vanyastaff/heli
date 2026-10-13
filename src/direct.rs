//! Runs a command in heli's place: its stdio, environment and exit status
//! become heli's own.
//!
//! On Unix heli `exec`s the command, so the process is the command. Elsewhere
//! heli waits for it and exits with its code; on Windows it first ignores
//! Ctrl-C and Ctrl-Break, which the command in the same console receives too,
//! so heli reports the command's exit instead of dying before it (cargo runs
//! its own external subcommands the same way).

use std::io;
use std::process::Command;

/// Exit code when the command does not exist (as a shell reports it).
const NOT_FOUND: i32 = 127;
/// Exit code when the command exists but cannot be started.
const NOT_STARTED: i32 = 126;

/// Runs `command` and exits with its status.
pub fn run(mut command: Command) -> ! {
    let error = replace(&mut command);
    eprintln!(
        "cargo-heli: cannot run `{}`: {error}",
        command.get_program().to_string_lossy()
    );
    std::process::exit(if error.kind() == io::ErrorKind::NotFound {
        NOT_FOUND
    } else {
        NOT_STARTED
    })
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
