//! Prints each argument on stdout as `arg: <debug>`, then, each only when its
//! variable is set:
//! - `ECHO_ENV` names a variable: `env <name>=<debug of its value>`;
//! - `ECHO_CWD`: `cwd <debug of the working directory>`;
//! - `ECHO_SESSION`: `session <where the process runs>`: on Windows the
//!   logon session id, window station and desktop, elsewhere the session id;
//! - `ECHO_STDIN`: reads stdin to its end, then `stdin <bytes read>`;
//! - `ECHO_ORPHAN` names a file: starts a copy of itself that keeps this
//!   process's stdout and stderr and waits for that file, and does not wait
//!   for it;
//! - `ECHO_WAIT_FOR` names a file: waits until it exists (60 s at most).
//!
//! Then it prints `echo-args: stderr` on stderr, and exits with `ECHO_EXIT`
//! (default 0).
//!
//! Standing in for a tool (as `cargo`, say), it answers version requests:
//! with `ECHO_VERSION` naming a file and `-V`, `-vV` or `--version` as its
//! last argument, it only prints that file's contents and exits 0, or exits
//! 1 when there is no such file.

use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    if let Some(version) = std::env::var_os("ECHO_VERSION")
        && std::env::args_os()
            .skip(1)
            .last()
            .is_some_and(|arg| arg == "-V" || arg == "-vV" || arg == "--version")
    {
        match std::fs::read(version) {
            Ok(text) => {
                std::io::Write::write_all(&mut std::io::stdout(), &text).expect("print");
                std::process::exit(0);
            }
            Err(_) => std::process::exit(1),
        }
    }
    for arg in std::env::args_os().skip(1) {
        println!("arg: {arg:?}");
    }
    if let Some(name) = std::env::var_os("ECHO_ENV") {
        let value = std::env::var_os(&name);
        println!("env {}={value:?}", name.to_string_lossy());
    }
    if std::env::var_os("ECHO_CWD").is_some() {
        let cwd = std::env::current_dir().expect("a working directory");
        println!("cwd {cwd:?}");
    }
    if std::env::var_os("ECHO_SESSION").is_some() {
        println!("session {}", session());
    }
    if std::env::var_os("ECHO_STDIN").is_some() {
        let mut input = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut input).expect("read stdin");
        println!("stdin {}", input.len());
    }
    if let Some(gate) = std::env::var_os("ECHO_ORPHAN") {
        #[allow(clippy::zombie_processes, reason = "the orphan outlives this process")]
        let _orphan = std::process::Command::new(std::env::current_exe().expect("own path"))
            .env_remove("ECHO_ORPHAN")
            .env_remove("ECHO_ENV")
            .env_remove("ECHO_CWD")
            .env_remove("ECHO_SESSION")
            .env_remove("ECHO_STDIN")
            .env("ECHO_WAIT_FOR", gate)
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("start the orphan");
    }
    if let Some(gate) = std::env::var_os("ECHO_WAIT_FOR") {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !Path::new(&gate).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    eprintln!("echo-args: stderr");
    let code = std::env::var("ECHO_EXIT")
        .ok()
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    std::process::exit(code);
}

#[cfg(windows)]
fn session() -> String {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcessId() -> u32;
        fn GetCurrentThreadId() -> u32;
        fn ProcessIdToSessionId(process: u32, session: *mut u32) -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetProcessWindowStation() -> *mut c_void;
        fn GetThreadDesktop(thread: u32) -> *mut c_void;
        fn GetUserObjectInformationW(
            object: *mut c_void,
            index: i32,
            info: *mut c_void,
            length: u32,
            needed: *mut u32,
        ) -> i32;
    }
    const UOI_NAME: i32 = 2;

    let name = |object: *mut c_void| {
        let mut buffer = [0u16; 256];
        let mut needed = 0;
        // SAFETY: the buffer and its length in bytes match.
        let ok = unsafe {
            GetUserObjectInformationW(
                object,
                UOI_NAME,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 2) as u32,
                &mut needed,
            )
        };
        assert!(ok != 0, "GetUserObjectInformationW failed");
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    };
    let mut id = 0;
    // SAFETY: plain queries about this process and thread.
    let (ok, station, desktop) = unsafe {
        (
            ProcessIdToSessionId(GetCurrentProcessId(), &mut id),
            GetProcessWindowStation(),
            GetThreadDesktop(GetCurrentThreadId()),
        )
    };
    assert!(ok != 0, "ProcessIdToSessionId failed");
    format!("{id} {}\\{}", name(station), name(desktop))
}

#[cfg(unix)]
fn session() -> String {
    unsafe extern "C" {
        fn getsid(pid: i32) -> i32;
    }
    // SAFETY: `getsid(0)` only queries this process.
    let id = unsafe { getsid(0) };
    assert!(id >= 0, "getsid failed");
    id.to_string()
}
