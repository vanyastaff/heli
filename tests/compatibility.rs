//! A client reads a newer daemon of the same protocol (DESIGN.md §3, "The
//! protocol"): fields and values a later daemon adds never keep an older
//! client from reading what it knows.

mod support;

use serde_json::Value;
use support::fake_daemon::{Answer, FakeDaemon};
use support::{Fixture, assert_success, runs};

/// The status a newer daemon answers: a run in a phase this client does not
/// know, of a class it does not know, with a field it does not know.
const NEWER_STATUS: &str = r#"{"Status":{"schema":1,
    "daemon":{"pid":4242,"protocol":1,"started_at":1791910980},
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
