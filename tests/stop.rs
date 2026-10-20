//! `cargo heli stop` and `restart`, and the daemon's version in `status`
//! (#87).

mod support;

use std::process::{Child, Command, Output, Stdio};

use support::{Fixture, assert_success, runs};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Starts `command` with its output captured.
fn start(command: &mut Command) -> Child {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli")
}

fn finish(child: Child) -> Output {
    child.wait_with_output().expect("wait for heli")
}

/// Starts a run that waits for the file `gate`, and returns it once the
/// daemon lists it, with the daemon's pid.
fn gated_run(fixture: &Fixture, gate: &std::path::Path) -> (Child, u32) {
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(fixture.echo_args())
        .arg("in flight")
        .env("ECHO_WAIT_FOR", gate);
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    (call, support::daemon_pid(&status).expect("a daemon"))
}

/// Starts a daemon with a run that ends at once, and returns its pid.
fn daemon(fixture: &Fixture) -> u32 {
    let output = fixture
        .heli(["exec", "--"])
        .arg(fixture.echo_args())
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    fixture.daemon_pid().expect("a daemon")
}

/// `status` names the daemon's version and program, in JSON and as text.
#[test]
fn status_shows_the_daemon_version() {
    let fixture = Fixture::new("basic");
    daemon(&fixture);
    let status = fixture.status();
    assert_eq!(
        status["daemon"]["version"],
        env!("CARGO_PKG_VERSION"),
        "{status}"
    );
    let program = status["daemon"]["executable"]
        .as_str()
        .expect("the daemon's program");
    assert!(program.contains("cargo-heli"), "{status}");

    let output = fixture.heli(["status"]).output().expect("run heli status");
    assert_success(&output, "cargo heli status");
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains(&format!("version {},", env!("CARGO_PKG_VERSION"))),
        "{stdout}"
    );
    assert!(!stdout.contains("older than"), "{stdout}");
}

/// `stop` hands over at once (docs/daemon.md, "Handover", #126): the
/// daemon stops listening and lets its run in flight finish; `status`
/// shows it retiring with that run; the next call starts a new daemon
/// meanwhile; the old one exits once its run has ended.
#[test]
fn stop_hands_over_and_lets_the_runs_end() {
    let fixture = Fixture::new("basic");
    let gate = fixture.path("gate");
    let (call, pid) = gated_run(&fixture, &gate);

    let output = fixture.heli(["stop"]).output().expect("run heli stop");
    assert_success(&output, "cargo heli stop");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains(&format!("(pid {pid}) hands over to a new daemon"))
            && stderr.contains("exits once its 1 run(s) in flight end"),
        "{stderr}"
    );
    let status = fixture.wait_for_status("the daemon to hand over", |status| {
        status["daemon"].is_null() && status["retiring"][0]["pid"] == pid
    });
    assert_eq!(
        status["retiring"][0]["runs"].as_array().map(Vec::len),
        Some(1),
        "{status}"
    );

    let replacement = daemon(&fixture);
    assert_ne!(replacement, pid);
    let status = fixture.status();
    assert_eq!(status["retiring"][0]["pid"], pid, "{status}");

    std::fs::write(&gate, "").expect("open the gate");
    let output = finish(call);
    assert_success(&output, "the run in flight");
    assert_eq!(text(&output.stdout), "arg: \"in flight\"\n");
    fixture.wait_for_status("the retiring daemon to exit", |status| {
        status["retiring"].as_array().is_some_and(Vec::is_empty)
    });
    assert_eq!(fixture.daemon_pid(), Some(replacement));
}

/// `restart` with a run in flight starts the new daemon at once; `cancel`
/// of the old daemon's run, asked of the new one, reaches it (#126).
#[test]
fn restart_under_load_and_cancel_through_the_new_daemon() {
    let fixture = Fixture::new("basic");
    let gate = fixture.path("gate");
    let (call, pid) = gated_run(&fixture, &gate);
    let run = runs(&fixture.status())[0]["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let output = fixture
        .heli(["restart"])
        .output()
        .expect("run heli restart");
    assert_success(&output, "cargo heli restart");
    let replacement = fixture.daemon_pid().expect("a new daemon");
    assert_ne!(replacement, pid, "{}", text(&output.stderr));

    let output = fixture
        .heli(["cancel", &run])
        .output()
        .expect("run heli cancel");
    assert_success(&output, "cargo heli cancel");
    let output = finish(call);
    assert!(!output.status.success(), "the run was cancelled");
    fixture.wait_for_status("the retiring daemon to exit", |status| {
        status["retiring"].as_array().is_some_and(Vec::is_empty)
    });
    assert_eq!(fixture.daemon_pid(), Some(replacement));
}

/// `stop --now --yes` cancels the runs in flight, and the daemon exits.
#[test]
fn stop_now_cancels_the_runs() {
    let fixture = Fixture::new("basic");
    let gate = fixture.path("gate");
    let (call, pid) = gated_run(&fixture, &gate);

    let output = fixture
        .heli(["stop", "--now", "--yes"])
        .output()
        .expect("run heli stop");
    assert_success(&output, "cargo heli stop --now --yes");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("cancelled r"), "{stderr}");
    let output = finish(call);
    assert!(!output.status.success(), "the run was cancelled");
    fixture.wait_for_status("the daemon to exit", |status| status["daemon"].is_null());
    assert_ne!(daemon(&fixture), pid);
}

/// Without a yes, `stop --now` stops nothing.
#[test]
fn stop_now_asks_first() {
    let fixture = Fixture::new("basic");
    let gate = fixture.path("gate");
    let (call, pid) = gated_run(&fixture, &gate);

    let output = fixture
        .heli(["stop", "--now"])
        .stdin(Stdio::null())
        .output()
        .expect("run heli stop");
    assert!(!output.status.success(), "declined");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("Cancel 1 run(s) in flight"), "{stderr}");
    assert!(stderr.contains("nothing stopped"), "{stderr}");
    assert_eq!(fixture.daemon_pid(), Some(pid));

    std::fs::write(&gate, "").expect("open the gate");
    assert_success(&finish(call), "the run in flight");
    assert_eq!(fixture.daemon_pid(), Some(pid), "it was not stopped");
}

/// `restart` replaces the daemon with a new one; without a daemon it starts
/// one; `stop` without one says so.
#[test]
fn restart_replaces_the_daemon() {
    let fixture = Fixture::new("basic");
    let output = fixture.heli(["stop"]).output().expect("run heli stop");
    assert_success(&output, "cargo heli stop without a daemon");
    assert!(
        text(&output.stderr).contains("no heli daemon is running"),
        "{}",
        text(&output.stderr)
    );

    let output = fixture
        .heli(["restart"])
        .output()
        .expect("run heli restart");
    assert_success(&output, "cargo heli restart without a daemon");
    let first = fixture.daemon_pid().expect("a daemon");

    let output = fixture
        .heli(["restart"])
        .output()
        .expect("run heli restart");
    assert_success(&output, "cargo heli restart");
    let stderr = text(&output.stderr);
    let second = fixture.daemon_pid().expect("a new daemon");
    assert_ne!(second, first, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "pid {second}, version {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{stderr}"
    );
}
