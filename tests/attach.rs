//! Running the same command while it is still in flight attaches to it:
//! the same working directory, argv and environment (apart from what a
//! shell keeps for itself). The client gets the run's output from its
//! first byte and its exit code; no second process starts.

mod support;

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

/// Polls `done` until it holds; panics after 60 s, naming `what` it waited
/// for.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What `cargo heli log <run>` prints on stdout.
fn logged(fixture: &Fixture, run: &str) -> String {
    let output = fixture.heli(["log", run]).output().expect("run heli log");
    assert_success(&output, "cargo heli log");
    text(&output.stdout)
}

/// The notice an attached client prints first, on stderr; panics without
/// one.
fn split_notice(stderr: &str) -> (&str, &str) {
    let (notice, rest) = stderr.split_once('\n').expect("a notice line");
    assert!(notice.starts_with("cargo-heli: attached to "), "{stderr}");
    (notice, rest)
}

/// How long a run goes before a client attaches to it, when the test reads
/// the age in the notice.
const MEASURABLE_AGE: Duration = Duration::from_millis(200);

/// A second call of the same command attaches to the first one's run: it
/// prints a notice, then the run's output from its start (written before it
/// attached), and exits with the run's code; the command started once.
/// What a shell keeps for itself (`PWD`, `OLDPWD`, `SHLVL`, `_`) may
/// differ. `status` counts the clients attached as they come and go.
#[test]
fn the_same_command_attaches_to_the_run_in_flight() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let starts = fixture.path("starts");
    let call = |shell: &str| {
        let mut call = fixture.heli(["exec", "--"]);
        call.arg(&echo)
            .arg("once")
            .env("ECHO_WAIT_FOR", &gate)
            .env("ECHO_STARTS", &starts)
            .env("ECHO_EXIT", "3");
        for name in ["PWD", "OLDPWD", "SHLVL", "_"] {
            call.env(name, format!("{name} of shell {shell}"));
        }
        start(&mut call)
    };
    let first = call("1");
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    // The run started before it was listed.
    let listed = Instant::now();
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    wait_until("the run's first output", || {
        logged(&fixture, &id) == "arg: \"once\"\n"
    });
    // The notice gives the age in tenths of a second: the run is old enough
    // that it shows.
    std::thread::sleep(MEASURABLE_AGE.saturating_sub(listed.elapsed()));

    let second = call("2");
    fixture.wait_for_status("the second client", |status| {
        runs(status).len() == 1 && runs(status)[0]["attached_clients"] == 2
    });
    let third = call("3");
    fixture.wait_for_status("the third client", |status| {
        runs(status)[0]["attached_clients"] == 3
    });
    support::kill_tree(third.id());
    let _ = finish(third);
    let status = fixture.wait_for_status("the third client to be gone", |status| {
        runs(status)[0]["attached_clients"] == 2
    });
    assert_eq!(runs(&status).len(), 1, "one run: {status}");

    std::fs::write(&gate, "").expect("open the gate");
    let first = finish(first);
    let second = finish(second);
    for output in [&first, &second] {
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        assert_eq!(text(&output.stdout), "arg: \"once\"\n");
        assert_eq!(support::split_footer(&output.stderr).1.run, id);
    }
    assert_eq!(
        support::without_footer(&first.stderr),
        "echo-args: stderr\n"
    );
    let stderr = support::without_footer(&second.stderr);
    let (notice, rest) = split_notice(&stderr);
    assert!(notice.contains(&id), "{notice}");
    // It says how long the run has been going: it may have started before
    // the files it reads were last changed.
    let age = notice
        .split_once("the same command started ")
        .and_then(|(_, rest)| rest.split_once(" s ago"))
        .and_then(|(seconds, _)| seconds.parse::<f64>().ok());
    assert!(
        age.is_some_and(|age| age >= MEASURABLE_AGE.as_secs_f64()),
        "{notice}"
    );
    assert_eq!(rest, "echo-args: stderr\n");
    let started = std::fs::read_to_string(&starts).expect("read the starts");
    assert_eq!(started.lines().count(), 1, "the command started once");
}

/// A daemon asked to retire starts no new run, but still attaches a client
/// to the same command in flight rather than leave it to run the command a
/// second time; it exits once both have heard the end.
#[test]
fn a_retiring_daemon_still_attaches_to_its_runs() {
    use std::io::{Read, Write};

    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let call = || {
        let mut call = fixture.heli(["exec", "--"]);
        call.arg(&echo).arg("in flight").env("ECHO_WAIT_FOR", &gate);
        start(&mut call)
    };
    let first = call();
    fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    // The handshake every version speaks, from a client of a newer protocol.
    let mut newer = support::endpoint::connect(&fixture.endpoint());
    newer.write_all(b"HELI").expect("send the magic");
    newer
        .write_all(&u32::MAX.to_le_bytes())
        .expect("send the version");
    newer.read_exact(&mut [0u8; 8]).expect("the daemon's hello");
    newer.write_all(b"R").expect("ask to retire");
    let _ = newer.read_to_end(&mut Vec::new());

    let second = call();
    fixture.wait_for_status("the second client", |status| {
        runs(status)[0]["attached_clients"] == 2
    });
    std::fs::write(&gate, "").expect("open the gate");
    for output in [finish(first), finish(second)] {
        assert_success(&output, "cargo heli exec");
        assert_eq!(text(&output.stdout), "arg: \"in flight\"\n");
    }
    fixture.wait_for_status("the daemon to exit", |status| status["daemon"].is_null());
}

/// On Windows, where variable names ignore case, a variable spelled in
/// another case is the same variable: the same command attaches.
#[cfg(windows)]
#[test]
fn variable_names_ignore_case_on_windows() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let call = |name: &str| {
        let mut call = fixture.heli(["exec", "--"]);
        call.arg(&echo).env("ECHO_WAIT_FOR", &gate).env(name, "on");
        start(&mut call)
    };
    let first = call("HELI_TEST_CASE");
    fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let second = call("heli_test_case");
    let status = fixture.wait_for_status("the second client", |status| {
        runs(status)[0]["attached_clients"] == 2
    });
    assert_eq!(runs(&status).len(), 1, "{status}");
    std::fs::write(&gate, "").expect("open the gate");
    for output in [finish(first), finish(second)] {
        assert_success(&output, "cargo heli exec");
    }
}

/// A run waiting for its worktree lock takes clients of the same command
/// too: an attached client hears what it waits for, then gets its output
/// once it has run, and the command runs once.
#[test]
fn a_run_waiting_for_its_worktree_takes_clients() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let starts = fixture.path("starts");
    // `echo-args` stands in for cargo: a build, held by the gate, and a
    // `fmt`, which waits for the build. Full output keeps the build's
    // command line as given.
    let mut build = fixture.heli(["--output", "full", "build"]);
    build.env("CARGO", &echo).env("ECHO_WAIT_FOR", &gate);
    let build = start(&mut build);
    let status = fixture.wait_for_status("the build", |status| runs(status).len() == 1);
    let build_id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    let fmt = || {
        let mut fmt = fixture.heli(["fmt"]);
        fmt.env("CARGO", &echo).env("ECHO_STARTS", &starts);
        start(&mut fmt)
    };
    let first = fmt();
    let status = fixture.wait_for_status("fmt to wait", |status| {
        runs(status).len() == 2 && runs(status)[1]["waiting_for"] == serde_json::json!([build_id])
    });
    let fmt_id = runs(&status)[1]["id"].as_str().expect("an id").to_owned();
    let second = fmt();
    fixture.wait_for_status("the second client", |status| {
        runs(status).len() == 2 && runs(status)[1]["attached_clients"] == 2
    });

    std::fs::write(&gate, "").expect("open the gate");
    assert_success(&finish(build), "the build");
    let first = finish(first);
    let second = finish(second);
    for output in [&first, &second] {
        assert_success(output, "cargo heli fmt");
        assert_eq!(text(&output.stdout), "arg: \"fmt\"\n");
        assert_eq!(support::split_footer(&output.stderr).1.run, fmt_id);
    }
    let stderr = support::without_footer(&second.stderr);
    let (notice, rest) = split_notice(&stderr);
    assert!(notice.contains(&fmt_id), "{notice}");
    let waiting = format!("cargo-heli: waiting for {build_id} (echo-args build) to finish");
    assert!(rest.starts_with(&waiting), "{stderr}");
    assert!(rest.ends_with("echo-args: stderr\n"), "{stderr}");
    let started = std::fs::read_to_string(&starts).expect("read the starts");
    assert_eq!(started.lines().count(), 1, "fmt ran once");
}

/// An agent whose call was killed mid-build calls the same command again:
/// it attaches to the build still going, and gets its whole output (from
/// before it attached too) and its exit code; no second build runs.
#[test]
fn a_call_again_after_a_killed_one_attaches_to_its_build() {
    let fixture = Fixture::new("basic");
    // Source-aware build sharing needs a complete inventory. Non-Git
    // commands intentionally run independently rather than attach stale work.
    std::fs::write(fixture.workspace().join(".gitignore"), "/target/\n").unwrap();
    for args in [vec!["init", "--quiet"], vec!["add", "-A"]] {
        let output = Command::new("git")
            .args(args)
            .current_dir(fixture.workspace())
            .output()
            .unwrap();
        assert_success(&output, "initialize build source inventory");
    }
    let gate = fixture.path("gate");
    // Full output: cargo's own lines show the build under way.
    let build = || {
        let mut build = fixture.heli(["--output", "full", "build", "-p", "gated"]);
        build.env("HELI_FIXTURE_GATE", &gate);
        start(&mut build)
    };
    let killed = build();
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    wait_until("the build to be under way", || {
        let logged = fixture.heli(["log", &id]).output().expect("run heli log");
        text(&logged.stderr).contains("Compiling gated v0.0.0")
    });
    support::kill_tree(killed.id());
    let _ = finish(killed);
    fixture.wait_for_status("the killed client to be gone", |status| {
        runs(status)[0]["attached_clients"] == 0
    });

    let again = build();
    let status = fixture.wait_for_status("the call again to attach", |status| {
        runs(status)[0]["attached_clients"] == 1
    });
    assert_eq!(runs(&status).len(), 1, "one build: {status}");
    std::fs::write(&gate, "").expect("open the gate");
    let again = finish(again);
    assert_success(&again, "the second call");
    let (stderr, footer) = support::split_footer(&again.stderr);
    assert_eq!(footer.run, id);
    let stderr = text(&stderr);
    let (_, output) = split_notice(&stderr);
    for part in ["Compiling gated v0.0.0", "Finished"] {
        assert!(output.contains(part), "{part:?} missing from {stderr}");
    }
}

/// A command that differs in argv, worktree, working directory or
/// environment is another command: each starts a run of its own.
#[test]
fn a_different_command_starts_a_run_of_its_own() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let other_worktree = fixture.path("other-worktree");
    std::fs::create_dir_all(&other_worktree).expect("create a worktree");
    std::fs::write(other_worktree.join(".git"), "gitdir: elsewhere\n").expect("mark it");
    let call = |arg: &str, configure: &dyn Fn(&mut Command)| {
        let mut call = fixture.heli(["exec", "--"]);
        call.arg(&echo).arg(arg).env("ECHO_WAIT_FOR", &gate);
        configure(&mut call);
        start(&mut call)
    };
    let mut calls = vec![call("same", &|_| {})];
    fixture.wait_for_status("the first run", |status| runs(status).len() == 1);
    calls.push(call("other", &|_| {}));
    calls.push(call("same", &|call| {
        call.current_dir(&other_worktree);
    }));
    calls.push(call("same", &|call| {
        call.current_dir(fixture.workspace().join("crates"));
    }));
    calls.push(call("same", &|call| {
        call.env("CARGO_TARGET_DIR", fixture.path("other-target"));
    }));

    let status = fixture.wait_for_status("five runs", |status| {
        let runs = runs(status);
        runs.len() == 5 && runs.iter().all(|run| run["attached_clients"] == 1)
    });
    // The same directory, however it is spelled (macOS's `/var` is
    // `/private/var`).
    let other_worktree = std::fs::canonicalize(&other_worktree).expect("the worktree exists");
    assert!(
        runs(&status).iter().any(|run| run["worktree"]
            .as_str()
            .and_then(|worktree| std::fs::canonicalize(worktree).ok())
            .is_some_and(|worktree| worktree == other_worktree)),
        "{status}"
    );
    std::fs::write(&gate, "").expect("open the gate");
    let mut ids = Vec::new();
    for call in calls {
        let output = finish(call);
        assert_success(&output, "cargo heli exec");
        let stderr = support::without_footer(&output.stderr);
        assert_eq!(stderr, "echo-args: stderr\n", "no notice");
        ids.push(support::split_footer(&output.stderr).1.run);
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "{ids:?}");
}
