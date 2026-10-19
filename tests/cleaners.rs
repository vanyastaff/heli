//! Tools that delete build output themselves (cargo-sweep, cargo-clean-all)
//! and heli's own eviction never delete at the same time (docs/runs.md,
//! "Tool profiles"): such a run holds heli's eviction lock throughout, and
//! waits for a pass in progress.

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use support::{Fixture, assert_success};

/// heli's eviction lock, as `cargo heli gc` and the daemon take it.
fn eviction_lock(fixture: &Fixture) -> File {
    std::fs::create_dir_all(fixture.state_dir()).expect("create the state directory");
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fixture.state_dir().join("evict.lock"))
        .expect("open the eviction lock")
}

/// `cargo heli sweep`, with `echo-args` as cargo, waiting for `gate`.
fn sweep(fixture: &Fixture, echo: &Path, gate: &Path, starts: &Path) -> Child {
    fixture
        .heli(["sweep", "--time", "30"])
        .env("CARGO", echo)
        .env("ECHO_WAIT_FOR", gate)
        .env("ECHO_STARTS", starts)
        .env("HELI_OUTPUT", "full")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli")
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// While cargo-sweep runs through heli, heli's eviction lock is taken: the
/// daemon's passes skip and `cargo heli gc` waits. heli says why first.
#[test]
fn a_cleaner_holds_the_eviction_lock_for_its_run() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let starts = fixture.path("starts");
    let child = sweep(&fixture, &echo, &gate, &starts);
    wait_for("the sweep to start", || starts.is_file());

    let lock = eviction_lock(&fixture);
    assert!(
        matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "the sweep holds heli's eviction lock"
    );
    std::fs::write(&gate, "").expect("open the gate");
    let output = child.wait_with_output().expect("wait for heli");
    assert_success(&output, "cargo heli sweep");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cargo-sweep sees only target/"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    lock.try_lock()
        .expect("the lock is free once the sweep ended");
}

/// A cleaner waits for an eviction pass in progress before it starts.
#[test]
fn a_cleaner_waits_for_an_eviction_pass() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    std::fs::write(&gate, "").expect("an open gate");
    let starts = fixture.path("starts");
    let pass = eviction_lock(&fixture);
    pass.lock().expect("a pass holds the lock");

    let child = sweep(&fixture, &echo, &gate, &starts);
    std::thread::sleep(Duration::from_secs(1));
    assert!(!starts.exists(), "the sweep waits for the pass");
    drop(pass);
    let output = child.wait_with_output().expect("wait for heli");
    assert_success(&output, "cargo heli sweep");
    assert!(starts.is_file(), "the sweep ran once the pass ended");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("waiting for heli's eviction pass"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
