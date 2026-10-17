//! The worktree lock (`DESIGN.md` §5): heli runs that read a worktree hold
//! its lock shared, mutate runs (`fmt`, `fix`, `clippy --fix`) hold it
//! exclusive. A run that has to wait says for which runs, and `status` shows
//! the wait.
//!
//! The runs held open here are real builds of the `gated` fixture crate,
//! whose build script waits for a gate file; real `cargo fmt` whose rustfmt
//! is `echo-args` waiting for a gate (`RUSTFMT`); or `echo-args` standing in
//! for cargo itself (`CARGO`), which heli runs with the subcommand's
//! arguments and classifies as it would cargo.

mod support;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::{Fixture, assert_success, runs};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Starts `command` with its output captured, in full output mode, so that
/// heli adds nothing to the arguments `status` lists and the waiting line
/// names (compact output adds cargo's JSON flags; `tests/output.rs`).
fn start(command: &mut Command) -> Child {
    command
        .env("HELI_OUTPUT", "full")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli")
}

/// Waits for `child` to exit, at most [`FINISH_WITHIN`]: a wait that is
/// never released fails the test instead of hanging it.
fn finish(child: Child) -> Output {
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(child.wait_with_output()));
    match receiver.recv_timeout(FINISH_WITHIN) {
        Ok(output) => output.expect("wait for heli"),
        Err(_) => {
            support::kill_tree(pid);
            panic!(
                "heli (pid {pid}) did not finish within {FINISH_WITHIN:?}: a wait that never ends?"
            );
        }
    }
}

/// How long a test waits for a run it no longer holds open.
const FINISH_WITHIN: Duration = Duration::from_secs(60);

fn open(gate: &Path) {
    std::fs::write(gate, "").expect("open the gate");
}

/// Makes `dir` a git worktree, so every directory below it is in it.
fn git_init(dir: &Path) {
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir)
        .output()
        .expect("run git init");
    assert_success(&output, "git init");
}

/// `cargo heli <args>` with `echo-args` as cargo: a run of the class `args`
/// give, which lasts until `gate` exists, or without one ends at once.
fn stand_in(fixture: &Fixture, echo: &Path, args: &[&str], gate: Option<&Path>) -> Child {
    let mut command = fixture.heli(args.iter().copied());
    command.env("CARGO", echo);
    if let Some(gate) = gate {
        command.env("ECHO_WAIT_FOR", gate);
    }
    start(&mut command)
}

/// A real `cargo heli build -p gated`, whose build lasts until `gate`
/// exists.
fn gated_build(fixture: &Fixture, gate: &Path) -> Child {
    let mut build = fixture.heli(["build", "-p", "gated"]);
    build.env("HELI_FIXTURE_GATE", gate);
    start(&mut build)
}

/// A real `cargo heli fmt` in `dir`, which lasts until `gate` exists: its
/// rustfmt is `echo-args`, waiting for the gate.
fn gated_fmt(fixture: &Fixture, echo: &Path, gate: &Path, dir: &Path) -> Child {
    let mut fmt = fixture.heli(["fmt"]);
    fmt.current_dir(dir)
        .env("RUSTFMT", echo)
        .env("ECHO_WAIT_FOR", gate);
    start(&mut fmt)
}

/// The run `status` lists whose arguments after the program are `args`.
fn listed<'a>(status: &'a Value, args: &[&str]) -> Option<&'a Value> {
    status["runs"].as_array()?.iter().find(|run| {
        run["argv"]
            .as_array()
            .is_some_and(|argv| argv.len() == args.len() + 1 && argv[1..] == args[..])
    })
}

/// Waits until `status` lists a run of `args` that waits for the worktree
/// lock (with `waiting` false: that does not wait), and returns its entry.
fn wait_until_listed(fixture: &Fixture, args: &[&str], waiting: bool) -> Value {
    let what = format!("`{}` to be listed, waiting: {waiting}", args.join(" "));
    let status = fixture.wait_for_status(&what, |status| {
        listed(status, args).is_some_and(|run| run["waiting_since"].is_u64() == waiting)
    });
    listed(&status, args).expect("listed").clone()
}

fn id(run: &Value) -> String {
    run["id"].as_str().expect("a run id").to_owned()
}

/// The line a run prints when it waits for `runs`, each an id and a
/// command.
fn waiting_line(runs: &[(&Value, &str)]) -> String {
    let runs: Vec<String> = runs
        .iter()
        .map(|(run, command)| format!("{} ({command})", id(run)))
        .collect();
    format!(
        "cargo-heli: waiting for {} to finish with this worktree",
        runs.join(", ")
    )
}

/// Waits for the first line `child` prints on stderr, at most
/// [`FINISH_WITHIN`], and returns it; [`finish`] still captures the rest.
///
/// A waiting run's line has to be heard before the test lets a run it
/// waits for end. `status` lists the run as waiting from its admission,
/// but the daemon names what it waits for only on reaching the lock, a
/// little later: a run that ends meanwhile is not named, or, if it was the
/// only one, the run says nothing at all.
fn first_stderr_line(child: &mut Child) -> String {
    let mut stderr = child.stderr.take().expect("stderr is captured");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A byte at a time, so that nothing after the line is taken.
        let mut line = Vec::new();
        let mut byte = [0u8];
        while stderr.read(&mut byte).is_ok_and(|read| read == 1) && byte[0] != b'\n' {
            line.push(byte[0]);
        }
        let _ = sender.send((line, stderr));
    });
    let (line, stderr) = receiver
        .recv_timeout(FINISH_WITHIN)
        .unwrap_or_else(|_| panic!("heli printed no line within {FINISH_WITHIN:?}"));
    child.stderr = Some(stderr);
    text(&line).trim_end_matches('\r').to_owned()
}

/// Whether heli said that the run waited.
fn said_waiting(output: &Output) -> bool {
    text(&output.stderr).contains("cargo-heli: waiting for")
}

/// When `path` was last written, in nanoseconds since 1970.
fn modified_ns(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .duration_since(UNIX_EPOCH)
        .expect("a time after 1970")
        .as_nanos()
}

/// `slow-a`'s source, as rustfmt would not leave it, and as it would.
const UNFORMATTED: &str = "pub const NAME:&str=\"slow-a\";\n";
const FORMATTED: &str = "pub const NAME: &str = \"slow-a\";\n";

/// Makes `slow-a`'s source unformatted, and returns its path.
fn unformatted_source(fixture: &Fixture) -> PathBuf {
    let path = fixture.workspace().join("crates/slow-a/src/lib.rs");
    std::fs::write(&path, UNFORMATTED).expect("write an unformatted source");
    path
}

/// AC: `fmt` started during a heli build of the same worktree waits for the
/// build to end, then runs. It says which run it waits for, and `status`
/// shows the wait.
#[test]
fn fmt_waits_for_a_build_of_its_worktree() {
    let fixture = Fixture::new("basic");
    git_init(&fixture.workspace());
    let source = unformatted_source(&fixture);
    let gate = fixture.path("gate");
    let build = gated_build(&fixture, &gate);
    let build_run = wait_until_listed(&fixture, &["build", "-p", "gated"], false);
    assert_eq!(build_run["class"], "compile", "{build_run}");

    // From a directory below the worktree's root: the same worktree.
    let mut fmt = fixture.heli(["fmt"]);
    fmt.current_dir(fixture.workspace().join("crates/slow-a"));
    let mut fmt = start(&mut fmt);
    let fmt_run = wait_until_listed(&fixture, &["fmt"], true);
    assert_eq!(fmt_run["class"], "mutate", "{fmt_run}");
    assert_eq!(fmt_run["waiting_for"], json!([id(&build_run)]));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    let since = fmt_run["waiting_since"].as_u64().expect("a time");
    assert!(
        (now - 120..=now + 5).contains(&since),
        "waiting_since {since} is not about now ({now})"
    );
    let status = fixture.heli(["status"]).output().expect("run heli status");
    let status = text(&status.stdout);
    let line = status
        .lines()
        .find(|line| line.contains(&format!(" {}  ", id(&fmt_run))))
        .unwrap_or_else(|| panic!("no line for fmt: {status}"));
    assert!(
        line.contains(&format!("mutate  waiting for {}", id(&build_run))),
        "{line:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&source).expect("read the source"),
        UNFORMATTED,
        "fmt must not rewrite the worktree during the build"
    );
    assert!(fmt.try_wait().expect("poll fmt").is_none(), "fmt waits");
    assert_eq!(
        first_stderr_line(&mut fmt),
        waiting_line(&[(&build_run, "cargo build -p gated")])
    );

    open(&gate);
    assert_success(&finish(build), "cargo heli build");
    let fmt = finish(fmt);
    assert_success(&fmt, "cargo heli fmt");
    assert_eq!(
        std::fs::read_to_string(&source).expect("read the source"),
        FORMATTED,
        "fmt ran after the build"
    );
    let built = fixture.target_dir().join("debug").join("libgated.rlib");
    assert!(
        modified_ns(&source) >= modified_ns(&built),
        "fmt rewrote the source before the build had finished"
    );
}

/// AC: a build started during `fmt` waits for it, and starts only once it
/// has finished.
#[test]
fn a_build_waits_for_fmt() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    git_init(&fixture.workspace());
    let gate = fixture.path("gate");
    let fmt = gated_fmt(&fixture, &echo, &gate, &fixture.workspace());
    let fmt_run = wait_until_listed(&fixture, &["fmt"], false);

    let log = fixture.concurrency_log();
    let mut build = fixture.heli(["build", "-p", "slow-a"]);
    log.attach(&mut build, 0);
    let mut build = start(&mut build);
    let build_run = wait_until_listed(&fixture, &["build", "-p", "slow-a"], true);
    assert_eq!(build_run["waiting_for"], json!([id(&fmt_run)]));
    assert!(log.intervals().is_empty(), "the build must not start yet");
    assert!(build.try_wait().expect("poll the build").is_none());
    assert_eq!(
        first_stderr_line(&mut build),
        waiting_line(&[(&fmt_run, "cargo fmt")])
    );

    open(&gate);
    assert_success(&finish(fmt), "cargo heli fmt");
    let build = finish(build);
    assert_success(&build, "cargo heli build");
    let intervals = log.intervals();
    assert_eq!(intervals.len(), 1, "{intervals:?}");
    assert!(
        intervals[0].start >= modified_ns(&gate),
        "the build started before fmt could finish"
    );
}

/// AC: builds in other worktrees do not wait for a worktree's `fmt`, and
/// neither does a `fmt` there.
#[test]
fn other_worktrees_do_not_wait() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let other = fixture.copy_workspace("other");
    git_init(&fixture.workspace());
    git_init(&other);
    let gate = fixture.path("gate");
    let mut fmt = gated_fmt(&fixture, &echo, &gate, &fixture.workspace());
    let fmt_run = wait_until_listed(&fixture, &["fmt"], false);

    for args in [&["build", "-p", "slow-a"][..], &["fmt"]] {
        let output = fixture
            .heli(args.iter().copied())
            .current_dir(&other)
            .env("CARGO_TARGET_DIR", fixture.path("other-target"))
            .output()
            .expect("run heli");
        assert_success(&output, &format!("{args:?} in another worktree"));
        assert!(!said_waiting(&output), "{}", text(&output.stderr));
    }
    assert!(
        fmt.try_wait().expect("poll fmt").is_none(),
        "fmt still runs"
    );
    let still = wait_until_listed(&fixture, &["fmt"], false);
    assert_eq!(id(&still), id(&fmt_run));

    open(&gate);
    assert_success(&finish(fmt), "cargo heli fmt");
}

/// AC: `fmt --check` only reads, so it does not wait for builds of its
/// worktree, whichever way `--check` is given.
#[test]
fn fmt_check_does_not_wait_for_builds() {
    let fixture = Fixture::new("basic");
    git_init(&fixture.workspace());
    unformatted_source(&fixture);
    let gate = fixture.path("gate");
    let mut build = gated_build(&fixture, &gate);
    wait_until_listed(&fixture, &["build", "-p", "gated"], false);

    for args in [&["fmt", "--check"][..], &["fmt", "--", "--check"]] {
        let output = fixture
            .heli(args.iter().copied())
            .output()
            .expect("run heli");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?} finds the unformatted source: {}",
            text(&output.stderr)
        );
        assert!(!said_waiting(&output), "{}", text(&output.stderr));
    }
    assert!(build.try_wait().expect("poll the build").is_none());

    open(&gate);
    assert_success(&finish(build), "cargo heli build");
}

/// Runs that only read a worktree share it; a mutate run waits for every
/// one of them, names them all, and starts once the last has finished.
#[test]
fn fmt_waits_for_every_run_of_its_worktree() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let (build_gate, exec_gate) = (fixture.path("build-gate"), fixture.path("exec-gate"));
    let build = stand_in(&fixture, &echo, &["build"], Some(&build_gate));
    let build_run = wait_until_listed(&fixture, &["build"], false);
    let mut exec = fixture.heli(["exec", "--"]);
    exec.arg(&echo)
        .arg("exec-arg")
        .env("ECHO_WAIT_FOR", &exec_gate);
    let exec = start(&mut exec);
    let exec_run = wait_until_listed(&fixture, &["exec-arg"], false);
    assert_eq!(exec_run["class"], "exec", "{exec_run}");

    let mut fmt = stand_in(&fixture, &echo, &["fmt"], None);
    let fmt_run = wait_until_listed(&fixture, &["fmt"], true);
    assert_eq!(
        fmt_run["waiting_for"],
        json!([id(&build_run), id(&exec_run)])
    );
    assert_eq!(
        first_stderr_line(&mut fmt),
        waiting_line(&[
            (&build_run, "echo-args build"),
            (&exec_run, "echo-args exec-arg")
        ])
    );

    open(&build_gate);
    assert_success(&finish(build), "the build");
    let fmt_run = wait_until_listed(&fixture, &["fmt"], true);
    assert_eq!(fmt_run["waiting_for"], json!([id(&exec_run)]));

    open(&exec_gate);
    assert_success(&finish(exec), "the exec run");
    assert_success(&finish(fmt), "fmt");
}

/// Runs take their turns at a worktree's lock in the order they came: a run
/// that comes while `fmt` waits for a build waits for `fmt`, rather than
/// joining the build and keeping `fmt` waiting.
#[test]
fn a_waiting_fmt_is_not_overtaken() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let (build_gate, fmt_gate) = (fixture.path("build-gate"), fixture.path("fmt-gate"));
    let build = stand_in(&fixture, &echo, &["build"], Some(&build_gate));
    wait_until_listed(&fixture, &["build"], false);
    let fmt = stand_in(&fixture, &echo, &["fmt"], Some(&fmt_gate));
    let fmt_run = wait_until_listed(&fixture, &["fmt"], true);
    let mut check = stand_in(&fixture, &echo, &["check"], None);
    let check_run = wait_until_listed(&fixture, &["check"], true);
    assert_eq!(check_run["waiting_for"], json!([id(&fmt_run)]));
    assert_eq!(
        first_stderr_line(&mut check),
        waiting_line(&[(&fmt_run, "echo-args fmt")])
    );

    open(&build_gate);
    assert_success(&finish(build), "the build");
    wait_until_listed(&fixture, &["fmt"], false);
    let check_run = wait_until_listed(&fixture, &["check"], true);
    assert_eq!(check_run["waiting_for"], json!([id(&fmt_run)]));

    open(&fmt_gate);
    assert_success(&finish(fmt), "fmt");
    assert_success(&finish(check), "the check");
}

/// A run asked for from inside another heli run does not wait for the
/// worktree lock, which the run around it holds: `cargo heli exec -- cargo
/// heli fmt` (a task runner that formats, say) would otherwise wait for
/// itself forever. Later runs still wait for it.
#[test]
fn a_run_inside_a_heli_run_does_not_wait_for_it() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut outer = fixture.heli(["exec", "--"]);
    outer
        .arg(support::heli_binary())
        .args(["fmt", "--inner"])
        .env("CARGO", &echo)
        .env("ECHO_WAIT_FOR", &gate);
    let outer = start(&mut outer);
    let status = fixture.wait_for_status("both runs", |status| runs(status).len() == 2);
    let inner_run = runs(&status)
        .into_iter()
        .find(|run| run["argv"][0] == echo.to_str().expect("a UTF-8 path"))
        .unwrap_or_else(|| panic!("no inner run: {status}"));
    assert_eq!(inner_run["class"], "mutate", "{inner_run}");
    assert_eq!(inner_run["waiting_for"], json!([]), "{status}");
    let check = stand_in(&fixture, &echo, &["check"], None);
    let check_run = wait_until_listed(&fixture, &["check"], true);
    assert_eq!(check_run["waiting_for"], json!([id(&inner_run)]));

    open(&gate);
    let outer = finish(outer);
    assert_success(&outer, "cargo heli exec -- cargo heli fmt");
    assert_eq!(text(&outer.stdout), "arg: \"fmt\"\narg: \"--inner\"\n");
    assert!(!said_waiting(&outer), "{}", text(&outer.stderr));
    assert_success(&finish(check), "the check");
}

/// On Windows, paths ignore case: a worktree entered in another case is the
/// same worktree, with the same lock.
#[cfg(windows)]
#[test]
fn a_worktree_in_another_case_is_the_same_worktree() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let build = stand_in(&fixture, &echo, &["build"], Some(&gate));
    let build_run = wait_until_listed(&fixture, &["build"], false);
    let shouted = PathBuf::from(fixture.workspace().to_string_lossy().to_uppercase());
    let mut fmt = fixture.heli(["fmt"]);
    fmt.current_dir(&shouted).env("CARGO", &echo);
    let fmt = start(&mut fmt);
    let fmt_run = wait_until_listed(&fixture, &["fmt"], true);
    assert_eq!(fmt_run["worktree"], shouted.to_str().expect("UTF-8"));
    assert_eq!(fmt_run["waiting_for"], json!([id(&build_run)]));

    open(&gate);
    assert_success(&finish(build), "the build");
    assert_success(&finish(fmt), "fmt");
}

/// One frame from the daemon: its kind (0 a message, 1 stdout, 2 stderr)
/// and its payload.
fn read_frame(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).expect("a frame");
    let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload).expect("a frame's payload");
    (header[0], payload)
}

/// A client older than run classes sends no class: its run counts as exec,
/// and the daemon does not tell it that it waits, in a message it could not
/// read.
#[test]
fn a_client_older_than_classes_is_served_as_exec() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let fmt = stand_in(&fixture, &echo, &["fmt"], Some(&gate));
    let fmt_run = wait_until_listed(&fixture, &["fmt"], false);

    // The handshake, then a run request as such a client sends it.
    let mut older = support::endpoint::connect(&fixture.endpoint());
    let mut hello = b"HELI".to_vec();
    hello.extend(support::fake_daemon::PROTOCOL.to_le_bytes());
    older.write_all(&hello).expect("send the hello");
    let mut answer = [0u8; 8];
    older.read_exact(&mut answer).expect("the daemon's hello");
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| name.eq_ignore_ascii_case("SystemRoot"))
        .collect();
    let request = json!({"Run": {
        "cwd": fixture.workspace().to_str().expect("a UTF-8 path"),
        "argv": [echo.to_str().expect("a UTF-8 path"), "older"],
        "env": env,
    }})
    .to_string();
    let mut frame = vec![0u8];
    frame.extend((request.len() as u32).to_le_bytes());
    frame.extend(request.as_bytes());
    older.write_all(&frame).expect("send the request");
    let (kind, started) = read_frame(&mut older);
    assert!(
        kind == 0 && text(&started).starts_with(r#"{"Started""#),
        "{}",
        text(&started)
    );
    let older_run = wait_until_listed(&fixture, &["older"], true);
    assert_eq!(older_run["class"], "exec", "{older_run}");
    assert_eq!(older_run["waiting_for"], json!([id(&fmt_run)]));

    open(&gate);
    assert_success(&finish(fmt), "fmt");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut frames = Vec::new();
        loop {
            let (kind, payload) = read_frame(&mut older);
            let exited = kind == 0 && text(&payload).contains("Exited");
            frames.push((kind, payload));
            if exited {
                break sender.send(frames);
            }
        }
    });
    let frames = receiver
        .recv_timeout(FINISH_WITHIN)
        .expect("the run's frames up to its exit");
    let messages: Vec<String> = frames
        .iter()
        .filter(|(kind, _)| *kind == 0)
        .map(|(_, payload)| text(payload))
        .collect();
    assert!(
        messages.iter().all(|message| !message.contains("Waiting")),
        "{messages:?}"
    );
    let stdout: Vec<u8> = frames
        .iter()
        .filter(|(kind, _)| *kind == 1)
        .flat_map(|(_, payload)| payload.clone())
        .collect();
    assert_eq!(text(&stdout), "arg: \"older\"\n");
}
