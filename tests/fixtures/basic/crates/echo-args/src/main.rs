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
//! - `ECHO_PID`: `pid <its process id>`;
//! - `ECHO_TREE` names a directory: starts a copy of itself in a process
//!   group of its own (as nextest starts each test), with no output, which
//!   waits for `ECHO_WAIT_FOR` too, and does not wait for it; each of the
//!   two writes an empty file named after its process id in that directory;
//! - `ECHO_WAIT_FOR` names a file: waits until it exists (60 s at most).
//!
//! First of all (after a version request, below), with `ECHO_STARTS` naming
//! a file, it appends a line there, so a test can count how many times it
//! started.
//!
//! Then it prints `echo-args: stderr` on stderr, and exits with `ECHO_EXIT`
//! (default 0), or with the code in the file `ECHO_EXIT_FILE` names when
//! that file exists.
//!
//! Copied under a tool's name (`cargo`, `rustc`), it stands in for that tool
//! and answers version requests: with `ECHO_VERSION` naming a directory and
//! `-V`, `-vV` or `--version` as its last argument, it only prints the file
//! in that directory named after the request (its own name, then the
//! arguments before the flag, joined by spaces: `cargo`, `cargo clippy`,
//! `rustc`) and exits 0, or exits 1 when there is no such file.

use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if let Some(versions) = std::env::var_os("ECHO_VERSION")
        && let Some((flag, before)) = args.split_last()
        && (flag == "-V" || flag == "-vV" || flag == "--version")
    {
        let program = std::env::current_exe().expect("own path");
        let mut request = program
            .file_stem()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        for arg in before {
            request.push(' ');
            request.push_str(&arg.to_string_lossy());
        }
        match std::fs::read(Path::new(&versions).join(request)) {
            Ok(text) => {
                std::io::Write::write_all(&mut std::io::stdout(), &text).expect("print");
                std::process::exit(0);
            }
            Err(_) => std::process::exit(1),
        }
    }
    if let Some(starts) = std::env::var_os("ECHO_STARTS") {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(starts)
            .and_then(|mut file| file.write_all(b"started\n"))
            .expect("append to ECHO_STARTS");
    }
    for arg in args {
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
    if std::env::var_os("ECHO_PID").is_some() {
        println!("pid {}", std::process::id());
    }
    if let Some(dir) = std::env::var_os("ECHO_TREE") {
        if std::env::var_os("ECHO_TREE_CHILD").is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().expect("own path"));
            child
                .env_remove("ECHO_STARTS")
                .env("ECHO_TREE_CHILD", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            std::os::unix::process::CommandExt::process_group(&mut child, 0);
            #[cfg(windows)]
            {
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
                std::os::windows::process::CommandExt::creation_flags(
                    &mut child,
                    CREATE_NEW_PROCESS_GROUP,
                );
            }
            #[allow(clippy::zombie_processes, reason = "the child outlives this process")]
            let _child = child.spawn().expect("start the child");
        }
        let file = Path::new(&dir).join(std::process::id().to_string());
        std::fs::write(file, "").expect("write the process id");
        if std::env::var_os("ECHO_TREE_CHILD").is_some() {
            // The child prints nothing: it only waits.
            if let Some(gate) = std::env::var_os("ECHO_WAIT_FOR") {
                wait_for(Path::new(&gate));
            }
            return;
        }
    }
    if let Some(gate) = std::env::var_os("ECHO_WAIT_FOR") {
        wait_for(Path::new(&gate));
    }
    eprintln!("echo-args: stderr");
    let from_file = std::env::var_os("ECHO_EXIT_FILE")
        .and_then(|file| std::fs::read_to_string(file).ok());
    let code = from_file
        .or_else(|| std::env::var("ECHO_EXIT").ok())
        .and_then(|code| code.trim().parse().ok())
        .unwrap_or(0);
    std::process::exit(code);
}

/// Waits until `gate` exists, 60 s at most.
fn wait_for(gate: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !gate.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
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
