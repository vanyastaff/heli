//! A jobserver client with work pending, as cc-rs or make is: it keeps
//! `JOBS_WORKERS` threads (default 1) busy, each taking a slot from the
//! jobserver in `CARGO_MAKEFLAGS`, holding it for `JOBS_HOLD_MS` (default
//! 50), giving it back, and after `JOBS_GAP_MS` (default 2) asking for the
//! next one. Unlike make, it has no job of its own outside the pool: every
//! worker holds a slot while it works.
//!
//! - `JOBS_PID` names a file: it writes its process id there first.
//! - `JOBS_IDLE` names a file: once it exists, the workers give their slots
//!   back and ask for no more, and the process lives on, idle.
//! - `JOBS_STOP` names a file: it exits once that file exists.
//! - It exits after `JOBS_MAX_MS` (default 120000) in any case, so a test
//!   that fails never leaves it behind for long.
//!
//! The jobserver is a semaphore on Windows (`--jobserver-auth=<name>`) and a
//! FIFO elsewhere (`--jobserver-auth=fifo:<path>`). Without one it exits
//! with 2.

use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    let millis = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .map_or(Duration::from_millis(default), Duration::from_millis)
    };
    let workers: u32 = std::env::var("JOBS_WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let hold = millis("JOBS_HOLD_MS", 50);
    let gap = millis("JOBS_GAP_MS", 2);
    let deadline = Instant::now() + millis("JOBS_MAX_MS", 120_000);
    if let Some(file) = std::env::var_os("JOBS_PID") {
        std::fs::write(file, std::process::id().to_string()).expect("write JOBS_PID");
    }
    let Some(auth) = std::env::var("CARGO_MAKEFLAGS").ok().and_then(|flags| {
        flags
            .split(' ')
            .filter_map(|flag| flag.strip_prefix("--jobserver-auth="))
            .last()
            .map(str::to_owned)
    }) else {
        eprintln!("jobs: no jobserver in CARGO_MAKEFLAGS");
        std::process::exit(2);
    };
    let idle = std::env::var_os("JOBS_IDLE");
    for _ in 0..workers {
        let slots = Slots::open(&auth);
        let idle = idle.clone();
        std::thread::spawn(move || {
            let idle = || idle.as_ref().is_some_and(|idle| Path::new(idle).exists());
            loop {
                slots.take();
                let held = Instant::now();
                while held.elapsed() < hold && !idle() {
                    std::thread::sleep(hold.min(Duration::from_millis(20)));
                }
                slots.give();
                if idle() {
                    return;
                }
                std::thread::sleep(gap);
            }
        });
    }
    let stop = std::env::var_os("JOBS_STOP");
    while Instant::now() < deadline && !stop.as_ref().is_some_and(|stop| Path::new(stop).exists())
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::process::exit(0);
}

/// One worker's connection to the jobserver.
#[cfg(windows)]
struct Slots(isize);

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenSemaphoreW(access: u32, inherit: i32, name: *const u16) -> isize;
    fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
    fn ReleaseSemaphore(handle: isize, count: i32, previous: *mut i32) -> i32;
}

#[cfg(windows)]
impl Slots {
    fn open(name: &str) -> Self {
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const SEMAPHORE_MODIFY_STATE: u32 = 0x0002;
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated.
        let handle = unsafe { OpenSemaphoreW(SYNCHRONIZE | SEMAPHORE_MODIFY_STATE, 0, wide.as_ptr()) };
        assert!(handle != 0, "jobs: cannot open the semaphore {name}");
        Self(handle)
    }

    fn take(&self) {
        const INFINITE: u32 = u32::MAX;
        // SAFETY: a semaphore handle with SYNCHRONIZE.
        let waited = unsafe { WaitForSingleObject(self.0, INFINITE) };
        assert_eq!(waited, 0, "jobs: cannot take a slot");
    }

    fn give(&self) {
        // SAFETY: a semaphore handle with SEMAPHORE_MODIFY_STATE.
        let released = unsafe { ReleaseSemaphore(self.0, 1, std::ptr::null_mut()) };
        assert!(released != 0, "jobs: cannot give a slot back");
    }
}

/// One worker's connection to the jobserver: the FIFO, opened for reading
/// and writing and blocking, as cargo's jobserver opens it.
#[cfg(unix)]
struct Slots(std::fs::File);

#[cfg(unix)]
impl Slots {
    fn open(auth: &str) -> Self {
        let path = auth
            .strip_prefix("fifo:")
            .unwrap_or_else(|| panic!("jobs: not a FIFO jobserver: {auth}"));
        let fifo = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|error| panic!("jobs: cannot open {path}: {error}"));
        Self(fifo)
    }

    fn take(&self) {
        use std::io::Read;
        (&self.0)
            .read_exact(&mut [0u8])
            .expect("jobs: cannot take a slot");
    }

    fn give(&self) {
        use std::io::Write;
        (&self.0)
            .write_all(b"|")
            .expect("jobs: cannot give a slot back");
    }
}
