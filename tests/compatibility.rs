//! A client reads a newer daemon of the same protocol (docs/daemon.md, "The
//! protocol"): fields and values a later daemon adds never keep an older
//! client from reading what it knows.

mod support;

use serde_json::Value;
use support::fake_daemon::{Answer, FakeDaemon};
use support::{Fixture, assert_success, runs};

/// The status a newer daemon answers: a run in a phase this client does not
/// know, of a class it does not know, with a field it does not know.
const NEWER_STATUS: &str = r#"{"Status":{"schema":1,
    "daemon":{"pid":4242,"protocol":2,"started_at":1791910980},
    "runs":[{"id":"r7","worktree":"/w","argv":["cargo","build"],
             "class":"profile","phase":"paused","started_at":1791911112,
             "paused_by":"r6"}]}}"#;

/// `status` reads a newer daemon's runs: a phase or a class it does not
/// know shows as `unknown` and `null` (`-` in the text form), and the rest
/// of the run as the daemon gave it.
#[test]
fn status_reads_a_phase_and_a_class_it_does_not_know() {
    let fixture = Fixture::new("basic");
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Reply(NEWER_STATUS));
    let output = fixture
        .heli(["status", "--json"])
        .output()
        .expect("run heli status");
    fake.received();
    assert_success(&output, "cargo heli status --json");
    assert!(
        output.stderr.is_empty(),
        "no warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let run = &runs(&status)[0];
    assert_eq!(run["id"], "r7", "{status}");
    assert_eq!(run["phase"], "unknown", "{status}");
    assert_eq!(run["class"], Value::Null, "{status}");
    assert_eq!(status["daemon"]["pid"], 4242, "{status}");

    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Reply(NEWER_STATUS));
    let output = fixture.heli(["status"]).output().expect("run heli status");
    fake.received();
    assert_success(&output, "cargo heli status");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines()
            .any(|line| line.contains("r7") && line.contains("cargo build  -  unknown")),
        "{text}"
    );
}

/// A run's client skips a message of a newer daemon's that it does not
/// know, a new kind or a known kind with fields it cannot read, and follows
/// the run to its end.
#[test]
fn a_run_skips_a_message_it_does_not_know() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let fake = FakeDaemon::start(
        &fixture.endpoint(),
        Answer::Replies(&[
            r#"{"Started":{"run":"r7","attached":null,"keyed_in":null}}"#,
            r#"{"Progress":{"percent":50}}"#,
            r#""Paused""#,
            r#"{"Waiting":{"runs":7}}"#,
            r#"{"Exited":{"code":3,"signal":null,"cancelled":false}}"#,
        ]),
    );
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    fake.received();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(!stderr.contains("lost"), "{stderr}");
    assert!(output.stdout.is_empty(), "nothing ran here: {output:?}");
}

/// Rejected execution inputs cannot fall back to a different command context.
#[test]
fn invalid_execution_inputs_never_fall_back_to_a_direct_run() {
    const FAILURE: &str = r#"{"SpawnFailed":{"not_found":false,"message":"cannot use dependency epoch: invalid inventory"}}"#;
    for answer in [
        Answer::Reply(FAILURE),
        Answer::Replies(&[r#""Preparing""#, FAILURE]),
    ] {
        let fixture = Fixture::new("basic");
        let echo = fixture.echo_args();
        let fake = FakeDaemon::start(&fixture.endpoint(), answer);
        let output = fixture
            .heli(["exec", "--"])
            .arg(echo)
            .output()
            .expect("run heli");
        fake.received();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(126), "{stderr}");
        assert!(stderr.contains("invalid inventory"), "{stderr}");
        assert!(!stderr.contains("running the command directly"), "{stderr}");
        assert!(output.stdout.is_empty(), "nothing ran here: {output:?}");
    }
}

/// Validation has not started a command, so loss there still permits fallback.
#[test]
fn losing_a_daemon_while_preparing_still_runs_the_command() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::LostAfter(&[r#""Preparing""#]));
    let output = fixture
        .heli(["exec", "--"])
        .arg(echo)
        .arg("preparation-loss-fallback-ran")
        .output()
        .expect("run heli");
    fake.received();
    assert_success(&output, "fallback before any command started");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("answered Preparing"), "{stderr}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("preparation-loss-fallback-ran"),
        "the command ran: {output:?}"
    );
}
