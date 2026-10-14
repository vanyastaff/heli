//! A daemon lost before a run's command started fails nothing (G5,
//! DESIGN.md §3, "A run"): the daemon says when the command has started
//! (`Spawned`), and until then a client that loses it runs the command
//! itself, after one warning. Once the command has started, a lost daemon
//! is an error: the command may have run.

mod support;

use std::process::{Child, Command, Output, Stdio};

use support::fake_daemon::{Answer, FakeDaemon};
use support::{Fixture, lines, runs};

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

/// Waits for `child` to exit, at most a minute.
fn finish(child: Child) -> Output {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("heli exits within a minute")
        .expect("wait for heli")
}

/// A daemon's `Started` that says `Spawned` follows.
const STARTED: &str =
    r#"{"Started":{"run":"r7","attached":null,"keyed_in":null,"spawned_follows":true}}"#;

/// Asserts that `output` is a direct run of `echo-args direct`, after one
/// warning that says so.
fn assert_ran_directly(output: &Output) {
    let stderr = lines(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr:?}");
    assert_eq!(text(&output.stdout), "arg: \"direct\"\n", "{stderr:?}");
    let warnings: Vec<&String> = stderr
        .iter()
        .filter(|line| line.starts_with("cargo-heli: warning: "))
        .collect();
    assert_eq!(warnings.len(), 1, "{stderr:?}");
    assert!(
        warnings[0].ends_with("; running the command directly"),
        "{stderr:?}"
    );
}

/// A daemon lost after `Started` but before `Spawned`: the command never
/// started there, so the client runs it.
#[test]
fn a_daemon_lost_before_the_command_started_means_a_direct_run() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::LostAfter(&[STARTED]));
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("direct")
        .output()
        .expect("run heli");
    fake.received();
    assert_ran_directly(&output);
}

/// A daemon lost after `Spawned`: the command may have run, so the client
/// exits 1 and runs nothing.
#[test]
fn a_daemon_lost_after_the_command_started_is_an_error() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let fake = FakeDaemon::start(
        &fixture.endpoint(),
        Answer::LostAfter(&[STARTED, r#""Spawned""#]),
    );
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("direct")
        .output()
        .expect("run heli");
    fake.received();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "nothing ran here: {output:?}");
    assert!(
        text(&output.stderr).contains("cargo-heli: lost the heli daemon during run r7"),
        "{output:?}"
    );
}

/// A daemon that does not say `Spawned` follows (one older than it) may
/// have started the command at any time after `Started`: lost, it is an
/// error, as before.
#[test]
fn a_daemon_without_spawned_lost_after_started_is_an_error() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let fake = FakeDaemon::start(
        &fixture.endpoint(),
        Answer::LostAfter(&[r#"{"Started":{"run":"r7","attached":null,"keyed_in":null}}"#]),
    );
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("direct")
        .output()
        .expect("run heli");
    fake.received();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "nothing ran here: {output:?}");
}

/// A run waiting for its worktree's lock has not started: when the daemon
/// dies then, its client runs the command itself. The run that held the
/// lock had started, and its client exits 1.
#[test]
fn a_run_waiting_for_its_worktree_runs_directly_when_the_daemon_is_lost() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut fmt = fixture.heli(["fmt"]);
    fmt.env("CARGO", &echo).env("ECHO_WAIT_FOR", &gate);
    let fmt = start(&mut fmt);
    fixture.wait_for_status("the fmt run", |status| runs(status).len() == 1);
    let mut waiting = fixture.heli(["exec", "--"]);
    waiting.arg(&echo).arg("direct");
    let waiting = start(&mut waiting);
    fixture.wait_for_status("the run waiting for the worktree", |status| {
        runs(status).iter().any(|run| {
            run["waiting_for"]
                .as_array()
                .is_some_and(|runs| !runs.is_empty())
        })
    });
    // The daemon alone, as a crash ends it.
    support::kill_process(fixture.daemon_pid().expect("a daemon"));
    let output = finish(waiting);
    std::fs::write(&gate, "").expect("open the gate");
    let fmt = finish(fmt);
    assert_ran_directly(&output);
    assert_eq!(fmt.status.code(), Some(1), "{fmt:?}");
    assert!(
        text(&fmt.stderr).contains("cargo-heli: lost the heli daemon during run r"),
        "{fmt:?}"
    );
}
