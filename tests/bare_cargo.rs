//! A bare cargo, started without heli but with the environment
//! `cargo heli setup` offers (DESIGN.md §4, "Reach"): while a daemon runs,
//! it draws from the same host pool as heli's runs; while none runs, it
//! warns and builds on its own, as if the environment were not there.

mod support;

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::{ConcurrencyLog, Fixture, Interval, assert_success};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The value of `CARGO_MAKEFLAGS` that `setup --print` offers, from its
/// first line.
fn offered_makeflags(fixture: &Fixture) -> String {
    let output = fixture
        .setup(None, ["--print"])
        .output()
        .expect("run heli setup --print");
    assert_success(&output, "cargo heli setup --print");
    let stdout = text(&output.stdout);
    stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("CARGO_MAKEFLAGS="))
        .unwrap_or_else(|| panic!("no CARGO_MAKEFLAGS line: {stdout}"))
        .to_owned()
}

/// `cargo heli ensure`: a daemon runs.
fn ensure(fixture: &Fixture) {
    let output = fixture.heli(["ensure"]).output().expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
}

/// A build of the three `slow-*` crates with `-j 3`, in the target
/// directory `target` of its own, its build scripts logging to `log` and
/// sleeping `sleep_ms`: `command` is plain cargo or heli.
fn slow_build(
    fixture: &Fixture,
    mut command: Command,
    target: &str,
    log: &ConcurrencyLog,
    sleep_ms: u64,
) -> Command {
    command
        .args([
            "build", "-j", "3", "-p", "slow-a", "-p", "slow-b", "-p", "slow-c",
        ])
        .env("CARGO_TARGET_DIR", fixture.path(target))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    log.attach(&mut command, sleep_ms);
    command
}

/// A bare cargo's [`slow_build`], with `CARGO_MAKEFLAGS` set to
/// `makeflags`.
fn bare_build(
    fixture: &Fixture,
    makeflags: &str,
    target: &str,
    log: &ConcurrencyLog,
    sleep_ms: u64,
) -> Command {
    let mut cargo = slow_build(
        fixture,
        fixture.cargo(Vec::<String>::new()),
        target,
        log,
        sleep_ms,
    );
    cargo.env("CARGO_MAKEFLAGS", makeflags);
    cargo
}

fn finish(child: Child, what: &str) -> Output {
    let output = child.wait_with_output().expect("wait for the build");
    assert_success(&output, what);
    output
}

/// The lines of cargo's stderr that warn.
fn warnings(output: &Output) -> Vec<String> {
    text(&output.stderr)
        .lines()
        .filter(|line| line.contains("warning"))
        .map(str::to_owned)
        .collect()
}

/// A bare cargo build with the environment setup offers, running beside a
/// heli build, draws from the same pool. With N = 2, and each cargo asked
/// for 3 jobs (`-j`, which a cargo that inherits a pool ignores, with a
/// warning), at most N + 2 of their build scripts run at once, one for each
/// cargo's own job; without the pool, 6 would. Neither waits for the other
/// to finish.
#[test]
fn a_bare_cargo_draws_from_the_pool_beside_a_heli_run() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let makeflags = offered_makeflags(&fixture);
    ensure(&fixture);

    let logs = [
        fixture.concurrency_log_named("heli"),
        fixture.concurrency_log_named("bare"),
    ];
    let heli = slow_build(
        &fixture,
        fixture.heli(Vec::<String>::new()),
        "target-heli",
        &logs[0],
        4000,
    )
    .spawn()
    .expect("start heli");
    let bare = bare_build(&fixture, &makeflags, "target-bare", &logs[1], 4000)
        .spawn()
        .expect("start cargo");
    finish(heli, "cargo heli build");
    let bare = finish(bare, "the bare cargo build");
    assert_eq!(
        warnings(&bare),
        [IN_THE_POOL],
        "cargo found the pool, and says so"
    );

    let scripts: Vec<Vec<Interval>> = logs.iter().map(ConcurrencyLog::intervals).collect();
    for (build, intervals) in scripts.iter().enumerate() {
        assert_eq!(intervals.len(), 3, "build {build}: {intervals:?}");
    }
    let most = support::max_overlap(&scripts.concat());
    assert!(
        most <= 2 + 2,
        "{most} build scripts ran at once: {scripts:?}"
    );
    for (build, other) in [(0, 1), (1, 0)] {
        let first_start = scripts[build].iter().map(|script| script.start).min();
        let last_end = scripts[other].iter().map(|script| script.end).max();
        assert!(
            first_start < last_end,
            "build {build} waited for build {other} to finish: {scripts:?}"
        );
    }
}

/// Whether the pool `name` exists: on Windows a semaphore by that name, on
/// Unix a file at that path.
#[cfg(windows)]
fn pool_exists(name: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenSemaphoreW, SYNCHRONIZATION_SYNCHRONIZE};

    let wide: Vec<u16> = name.encode_wide().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated; a handle opened is closed at once.
    unsafe {
        let handle = OpenSemaphoreW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide.as_ptr());
        if handle.is_null() {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

#[cfg(unix)]
fn pool_exists(name: &OsStr) -> bool {
    std::fs::symlink_metadata(name).is_ok()
}

/// A process other than the daemon that holds the pool open with a slot
/// taken, as a bare cargo does while it builds; dropping it gives the slot
/// back and closes the pool.
struct Holder {
    #[cfg(windows)]
    semaphore: std::os::windows::io::OwnedHandle,
    #[cfg(unix)]
    fifo: std::fs::File,
}

impl Holder {
    /// Opens the pool `name` as cargo's jobserver does, and takes a slot.
    #[cfg(windows)]
    fn take(name: &OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{
            OpenSemaphoreW, SEMAPHORE_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
            WaitForSingleObject,
        };

        let wide: Vec<u16> = name.encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated; the handle is owned below.
        let handle = unsafe {
            OpenSemaphoreW(
                SYNCHRONIZATION_SYNCHRONIZE | SEMAPHORE_MODIFY_STATE,
                0,
                wide.as_ptr(),
            )
        };
        assert!(!handle.is_null(), "open the semaphore");
        // SAFETY: a semaphore handle just opened.
        assert_eq!(
            unsafe { WaitForSingleObject(handle, 0) },
            WAIT_OBJECT_0,
            "take a slot"
        );
        Self {
            // SAFETY: the handle is this holder's from here on.
            semaphore: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
        }
    }

    /// Opens the pool `name` as cargo's jobserver does, and takes a slot.
    #[cfg(unix)]
    fn take(name: &OsStr) -> Self {
        let mut fifo = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .expect("open the FIFO");
        fifo.read_exact(&mut [0u8]).expect("take a slot");
        Self { fifo }
    }
}

impl Drop for Holder {
    #[cfg(windows)]
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: a semaphore handle with SEMAPHORE_MODIFY_STATE.
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseSemaphore(
                self.semaphore.as_raw_handle(),
                1,
                std::ptr::null_mut(),
            );
        }
    }

    #[cfg(unix)]
    fn drop(&mut self) {
        let _ = (&self.fifo).write_all(b"|");
    }
}

/// Polls `done` until it holds; panics after 10 s, naming `what` it waited
/// for.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Asks the fixture's daemon to retire, as a client of a newer protocol
/// does, and waits until it has exited (it has no runs to finish).
fn retire(fixture: &Fixture) {
    let mut newer = support::endpoint::connect(&fixture.endpoint());
    newer.write_all(b"HELI").expect("send the magic");
    newer
        .write_all(&u32::MAX.to_le_bytes())
        .expect("send the version");
    let mut answer = [0u8; 8];
    newer.read_exact(&mut answer).expect("the daemon's hello");
    newer.write_all(b"R").expect("ask to retire");
    let _ = newer.read_to_end(&mut Vec::new());
    fixture.wait_for_status("the daemon to exit", |status| status["daemon"].is_null());
}

/// A bare cargo build with the environment setup offers, while no daemon
/// runs: before any daemon ran, after one was killed, and after one
/// retired. The pool is gone once its daemon is (on Unix the FIFO would
/// otherwise stay, with no slot in it). So the build succeeds with one
/// warning, cargo's own about the jobserver, and runs its jobs side by side
/// in a pool of its own, as without the environment.
#[test]
fn a_bare_cargo_without_a_daemon_builds_on_its_own_after_a_warning() {
    let fixture = Fixture::new("basic");
    let makeflags = offered_makeflags(&fixture);
    let pool = fixture.pool();
    let stops: [(&str, &dyn Fn()); 3] = [
        ("before any daemon", &|| {}),
        ("after a daemon was killed", &|| {
            ensure(&fixture);
            let daemon = fixture.daemon_pid().expect("a daemon");
            support::kill_process(daemon);
            fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());
        }),
        ("after a daemon retired", &|| {
            ensure(&fixture);
            retire(&fixture);
        }),
    ];
    for (case, (when, stop)) in stops.iter().enumerate() {
        stop();
        wait_until(&format!("the pool to be gone {when}"), || {
            !pool_exists(&pool)
        });
        let log = fixture.concurrency_log_named(&format!("case-{case}"));
        let output = bare_build(&fixture, &makeflags, &format!("target-{case}"), &log, 3000)
            .output()
            .expect("run cargo");
        assert_success(&output, when);
        let warnings = warnings(&output);
        assert_eq!(warnings.len(), 1, "{when}: {warnings:?}");
        assert!(
            warnings[0].starts_with("warning: failed to connect to jobserver"),
            "{when}: {warnings:?}"
        );
        let scripts = log.intervals();
        assert_eq!(scripts.len(), 3, "{when}: {scripts:?}");
        assert!(
            log.max_concurrency() >= 2,
            "{when}: one job at a time: {scripts:?}"
        );
    }
}

/// The warning a bare cargo prints when it found a pool to draw from: its
/// `-j` is ignored.
const IN_THE_POOL: &str = "warning: a `-j` argument was passed to Cargo but Cargo is also \
                           configured with an external jobserver in its environment, ignoring \
                           the `-j` parameter";

/// A pool that some process still holds when its daemon dies lives on
/// while it is held, slots and all, as it does for a restarted daemon to
/// adopt: a bare cargo still draws from it. Once nobody holds it, it is
/// gone (on Unix, the FIFO is removed), and a bare cargo builds on its own.
#[test]
fn a_held_pool_outlives_its_daemon_until_released() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let makeflags = offered_makeflags(&fixture);
    let pool = fixture.pool();
    ensure(&fixture);
    let holder = Holder::take(&pool);
    support::kill_process(fixture.daemon_pid().expect("a daemon"));
    fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());

    std::thread::sleep(Duration::from_secs(1));
    assert!(pool_exists(&pool), "held, the pool lives on");
    let log = fixture.concurrency_log_named("held");
    let output = bare_build(&fixture, &makeflags, "target-held", &log, 0)
        .output()
        .expect("run cargo");
    assert_success(&output, "a bare cargo while the pool is held");
    assert_eq!(warnings(&output), [IN_THE_POOL]);

    drop(holder);
    wait_until("the pool to be gone", || !pool_exists(&pool));
    let log = fixture.concurrency_log_named("released");
    let output = bare_build(&fixture, &makeflags, "target-released", &log, 0)
        .output()
        .expect("run cargo");
    assert_success(&output, "a bare cargo once the pool is released");
    let warnings = warnings(&output);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("warning: failed to connect to jobserver"),
        "{warnings:?}"
    );
}

/// A pool that a new daemon adopted stays after the process that held it
/// lets go: it is that daemon's now, and a bare cargo draws from it.
#[test]
fn a_pool_a_new_daemon_adopted_stays() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let makeflags = offered_makeflags(&fixture);
    let pool = fixture.pool();
    ensure(&fixture);
    let holder = Holder::take(&pool);
    let first = fixture.daemon_pid().expect("a daemon");
    support::kill_process(first);
    fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());
    ensure(&fixture);
    assert_ne!(fixture.daemon_pid(), Some(first), "a new daemon");

    drop(holder);
    std::thread::sleep(Duration::from_secs(2));
    assert!(pool_exists(&pool), "the new daemon's pool stays");
    assert_eq!(
        fixture.status()["pool"]["free"],
        2,
        "with both slots free again"
    );
    let log = fixture.concurrency_log_named("adopted");
    let output = bare_build(&fixture, &makeflags, "target-adopted", &log, 0)
        .output()
        .expect("run cargo");
    assert_success(&output, "a bare cargo");
    assert_eq!(warnings(&output), [IN_THE_POOL]);
}
