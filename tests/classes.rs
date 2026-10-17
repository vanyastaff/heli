//! Run classes: each run is classified by its subcommand and arguments
//! (`DESIGN.md` §5, "Classes"), and `status` shows the class.

mod support;

use std::collections::BTreeMap;
use std::process::{Child, Stdio};

use serde_json::{Value, json};
use support::fake_daemon::{Answer, FakeDaemon};
use support::{Fixture, assert_success, runs};

/// The status a daemon older than run classes answers: its runs have no
/// class and no wait.
const OLDER_STATUS: &str = r#"{"Status":{"schema":1,
    "daemon":{"pid":4242,"protocol":1,"started_at":1791910980},
    "runs":[{"id":"r7","worktree":"/w","argv":["cargo","build"],
             "phase":"running","started_at":1791911112}]}}"#;

/// `status` still reads a daemon older than run classes: its runs show no
/// class (`null`, and `-` in the text form) and no wait.
#[test]
fn status_reads_a_daemon_older_than_classes() {
    let fixture = Fixture::new("basic");
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Reply(OLDER_STATUS));
    let status = fixture.status();
    fake.received();
    let run = &runs(&status)[0];
    assert_eq!(run["id"], "r7", "{status}");
    assert_eq!(run["class"], Value::Null, "{status}");
    assert_eq!(run["waiting_since"], Value::Null, "{status}");
    assert_eq!(run["waiting_for"], json!([]), "{status}");

    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Reply(OLDER_STATUS));
    let output = fixture.heli(["status"]).output().expect("run heli status");
    fake.received();
    assert_success(&output, "cargo heli status");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines()
            .any(|line| line.contains("r7") && line.contains("cargo build  -  running")),
        "{text}"
    );
}

/// Every reading of a command line the classes depend on, run by heli with
/// `echo-args` as its cargo, and listed by `status` with the class it got.
#[test]
fn status_shows_each_runs_class() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let cases: &[(&[&str], &str)] = &[
        (&["build", "-p", "a"], "compile"),
        (&["b"], "compile"),
        (&["run"], "compile"),
        (&["r"], "compile"),
        (&["doc"], "compile"),
        (&["d"], "compile"),
        (&["check"], "verify"),
        (&["c"], "verify"),
        (&["clippy"], "verify"),
        (&["clippy", "--", "--fix"], "verify"),
        (&["fmt", "--check"], "verify"),
        (&["fmt", "--all", "--", "--check"], "verify"),
        (&["test"], "test"),
        (&["t"], "test"),
        (&["nextest", "run", "-p", "a"], "test"),
        (&["nextest", "r"], "test"),
        (&["fmt"], "mutate"),
        (&["fmt", "--all"], "mutate"),
        (&["fix", "--allow-dirty"], "mutate"),
        (&["clippy", "--fix"], "mutate"),
        (
            &["clippy", "--all-targets", "--fix", "--", "-D", "warnings"],
            "mutate",
        ),
        (&["+nightly", "fmt"], "mutate"),
        (&["+nightly", "check"], "verify"),
        (&["bench"], "exec"),
        (&["nextest", "list"], "exec"),
        (&["xtask", "fmt"], "exec"),
        (&["add", "serde"], "mutate"),
        (&["remove", "serde"], "mutate"),
        (&["update"], "mutate"),
        (&["hakari", "generate"], "mutate"),
        (&["hakari", "verify"], "verify"),
        (&["machete", "--fix"], "mutate"),
        (&["machete"], "verify"),
        (&["shear", "--fix"], "mutate"),
        (&["insta", "accept"], "mutate"),
        (&["sqlx", "prepare"], "mutate"),
        (&["hack", "--each-feature", "check"], "verify"),
        (&["hack", "test"], "exec"),
        (&["+nightly", "udeps"], "verify"),
        (&["public-api"], "verify"),
        (&["audit"], "exec"),
        (&["deny", "check"], "exec"),
        (&["audit", "--no-fetch"], "verify"),
        (&["deny", "check", "--disable-fetch"], "verify"),
        (&["deny", "--offline", "check"], "verify"),
        (&["deny", "check", "licenses", "bans"], "verify"),
        (&["deny", "check", "advisories"], "exec"),
        (&["upgrade"], "mutate"),
        (&["set-version", "1.2.0"], "mutate"),
        (&["release", "patch"], "mutate"),
        (&["sort", "--check"], "verify"),
        (&["sort", "-w"], "mutate"),
        (&["outdated"], "exec"),
    ];
    let gate = fixture.path("gate");
    let start = |args: &[&str], exec: bool| -> Child {
        let mut command = if exec {
            let mut command = fixture.heli(["exec", "--"]);
            command.arg(&echo);
            command
        } else {
            fixture.heli(Vec::<String>::new())
        };
        // Full output, so that heli adds nothing to the arguments `status`
        // lists (compact output adds cargo's JSON flags; `tests/output.rs`).
        command
            .args(args)
            .env("CARGO", &echo)
            .env("ECHO_WAIT_FOR", &gate)
            .env("HELI_OUTPUT", "full")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start heli")
    };
    let mut calls: Vec<Child> = cases.iter().map(|(args, _)| start(args, false)).collect();
    // `exec` is its own class, whatever the command: here, one that looks
    // like `cargo fmt`.
    calls.push(start(&["fmt", "--through-exec"], true));

    let status = fixture.wait_for_status("every run to be listed", |status| {
        runs(status).len() == calls.len()
    });
    let classes: BTreeMap<String, String> = runs(&status)
        .iter()
        .map(|run| {
            let args: Vec<&str> = run["argv"].as_array().expect("an argv")[1..]
                .iter()
                .map(|arg| arg.as_str().expect("a string"))
                .collect();
            let class = run["class"].as_str().unwrap_or_else(|| panic!("{run}"));
            (args.join(" "), class.to_owned())
        })
        .collect();
    let mut expected: BTreeMap<String, String> = cases
        .iter()
        .map(|(args, class)| (args.join(" "), (*class).to_owned()))
        .collect();
    expected.insert("fmt --through-exec".to_owned(), "exec".to_owned());
    std::fs::write(&gate, "").expect("open the gate");
    for call in calls {
        let output = call.wait_with_output().expect("wait for heli");
        assert_success(&output, "a classified run");
    }
    assert_eq!(classes, expected);
}
