//! `cargo heli` through its daemon: the first call starts a per-user daemon,
//! later calls reuse it, and the daemon runs each command for its client.

mod support;

use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use support::fake_daemon::{Answer, FakeDaemon};
use support::{Fixture, assert_success, lines, runs};

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

fn open(gate: &std::path::Path) {
    std::fs::write(gate, "").expect("open the gate");
}

/// While a run is in flight, `status --json` lists it with its id, its
/// worktree (the repository root above the working directory), argv, phase
/// and start time; once it has finished, it is gone.
#[test]
fn status_lists_a_run_while_it_runs() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let git = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(fixture.workspace())
        .output()
        .expect("run git init");
    assert_success(&git, "git init");
    let gate = fixture.path("gate");
    let mut run = fixture.heli(["exec", "--"]);
    run.arg(&echo)
        .arg("first arg")
        .env("ECHO_WAIT_FOR", &gate)
        .current_dir(fixture.workspace().join("crates"));
    let run = start(&mut run);

    let status = fixture.wait_for_status("the run to be listed", |status| runs(status).len() == 1);
    assert_eq!(status["schema"], 1, "{status}");
    let listed = &runs(&status)[0];
    let id = listed["id"].as_str().expect("a run id");
    assert!(
        id.starts_with('r') && id[1..].parse::<u64>().is_ok(),
        "run id {id:?}"
    );
    let worktree = listed["worktree"].as_str().expect("a worktree");
    assert_eq!(
        std::fs::canonicalize(worktree).expect("the worktree exists"),
        std::fs::canonicalize(fixture.workspace()).expect("the workspace exists"),
    );
    assert_eq!(
        listed["argv"],
        serde_json::json!([echo.to_str().expect("a UTF-8 path"), "first arg"])
    );
    assert_eq!(listed["phase"], "running");
    let started = listed["started_at"]
        .as_u64()
        .expect("a start time in seconds since 1970");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    assert!(
        (now - 120..=now + 5).contains(&started),
        "started_at {started} is not about now ({now})"
    );
    let daemon_started = status["daemon"]["started_at"].as_u64().expect("a number");
    assert!(
        (now - 120..=started).contains(&daemon_started),
        "the daemon started before the run: {status}"
    );

    open(&gate);
    let output = finish(run);
    assert_success(&output, "cargo heli exec");
    assert_eq!(text(&output.stdout), "arg: \"first arg\"\n");
    assert!(
        fixture.runs().is_empty(),
        "a finished run is no longer listed"
    );
}

/// The run happens where the client is, with the client's environment and
/// a program path relative to the client's directory, and its output and
/// exit code are those of direct execution.
#[test]
fn a_run_matches_direct_execution() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let name = echo.file_name().expect("a file name");
    let dir = fixture.workspace().join("crates");
    std::fs::create_dir_all(dir.join("bin")).expect("create bin");
    std::fs::copy(&echo, dir.join("bin").join(name)).expect("copy echo-args");
    let relative = std::path::Path::new("bin").join(name);
    let configure = |command: &mut Command| {
        command
            .arg("an arg")
            .current_dir(&dir)
            .env("ECHO_CWD", "1")
            .env("ECHO_ENV", "HELI_TEST_VALUE")
            .env("HELI_TEST_VALUE", "from the client")
            .env("ECHO_EXIT", "5");
    };
    let mut direct = fixture.command(dir.join("bin").join(name), Vec::<String>::new());
    configure(&mut direct);
    let direct = direct.output().expect("run echo-args");
    let mut heli = fixture.heli(["exec", "--"]);
    heli.arg(&relative);
    configure(&mut heli);
    let heli = heli.output().expect("run heli");

    assert_eq!(heli.status.code(), Some(5));
    assert_eq!(heli.status.code(), direct.status.code());
    assert_eq!(text(&heli.stdout), text(&direct.stdout));
    assert!(
        text(&heli.stdout).contains("env HELI_TEST_VALUE=Some(\"from the client\")"),
        "{}",
        text(&heli.stdout)
    );
    assert_eq!(support::without_footer(&heli.stderr), text(&direct.stderr));
    assert!(fixture.daemon_pid().is_some(), "the daemon ran it");
}

/// Two calls started together, with no daemon running yet, start one daemon
/// between them, which serves both at once.
#[test]
fn one_daemon_serves_two_concurrent_calls() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let calls: Vec<Child> = ["a", "b"]
        .into_iter()
        .map(|arg| {
            let mut call = fixture.heli(["exec", "--"]);
            call.arg(&echo).arg(arg).env("ECHO_WAIT_FOR", &gate);
            start(&mut call)
        })
        .collect();

    let status =
        fixture.wait_for_status("both runs to be listed", |status| runs(status).len() == 2);
    let mut args: Vec<String> = runs(&status)
        .iter()
        .map(|run| run["argv"][1].as_str().expect("an argument").to_owned())
        .collect();
    args.sort();
    assert_eq!(args, ["a", "b"], "one daemon lists both runs: {status}");

    open(&gate);
    for (call, arg) in calls.into_iter().zip(["a", "b"]) {
        let output = finish(call);
        assert_success(&output, "cargo heli exec");
        assert_eq!(text(&output.stdout), format!("arg: {arg:?}\n"));
        assert_eq!(
            support::without_footer(&output.stderr),
            "echo-args: stderr\n",
            "no fallback warning"
        );
    }
    assert_eq!(
        fixture.daemon_pid(),
        support::daemon_pid(&status),
        "the same daemon"
    );
}

/// A daemon killed while idle is replaced by the next call, which runs
/// normally; run ids are not reused across daemons.
#[test]
fn a_killed_daemon_is_replaced_by_the_next_call() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let run_id = |gate: &str| {
        let gate = fixture.path(gate);
        let mut call = fixture.heli(["exec", "--"]);
        call.arg(&echo).env("ECHO_WAIT_FOR", &gate);
        let call = start(&mut call);
        let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
        open(&gate);
        let output = finish(call);
        assert_success(&output, "cargo heli exec");
        assert_eq!(
            support::without_footer(&output.stderr),
            "echo-args: stderr\n",
            "no warning"
        );
        (
            runs(&status)[0]["id"].as_str().expect("an id").to_owned(),
            support::daemon_pid(&status).expect("a daemon"),
        )
    };
    let (first_run, first_daemon) = run_id("gate-1");

    support::kill_tree(first_daemon);
    fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());

    let (second_run, second_daemon) = run_id("gate-2");
    assert_ne!(second_daemon, first_daemon, "a new daemon");
    assert_ne!(second_run, first_run, "a new run id");
}

/// The daemon is not part of the process tree of the call that started it:
/// killing that call with its descendants, as an agent harness does when it
/// stops a command, leaves the daemon and its run going.
#[test]
fn the_daemon_outlives_the_call_that_started_it() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).env("ECHO_WAIT_FOR", &gate);
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let daemon = support::daemon_pid(&status).expect("a daemon");

    support::kill_tree(call.id());
    let _ = finish(call);
    let status = fixture.status();
    assert_eq!(
        support::daemon_pid(&status),
        Some(daemon),
        "the daemon survived: {status}"
    );
    assert_eq!(runs(&status).len(), 1, "its run goes on: {status}");
    open(&gate);
    fixture.wait_for_status("the run to finish", |status| {
        runs(status).iter().all(|run| run["phase"] == "done")
    });
}

/// `echo-args` with `args`, run by `heli` and directly: `heli` printed
/// exactly one warning, the one that it ran the command directly, and
/// otherwise matches direct execution. Returns the warning.
fn assert_ran_directly_with_one_warning(fixture: &Fixture, heli: &mut Command) -> String {
    let echo = fixture.echo_args();
    let args = ["one", "two words"];
    let heli = heli
        .args(["exec", "--"])
        .arg(&echo)
        .args(args)
        .env("ECHO_EXIT", "7")
        .output()
        .expect("run heli");
    let direct = fixture
        .command(&echo, args)
        .env("ECHO_EXIT", "7")
        .output()
        .expect("run echo-args");
    assert_eq!(heli.status.code(), direct.status.code());
    assert_eq!(text(&heli.stdout), text(&direct.stdout));
    let stderr = lines(&heli.stderr);
    let (warning, rest) = stderr.split_first().expect("a warning");
    assert!(
        warning.starts_with("cargo-heli: warning: ")
            && warning.ends_with("; running the command directly"),
        "one warning line first: {stderr:?}"
    );
    assert_eq!(rest, lines(&direct.stderr), "and nothing else from heli");
    warning.clone()
}

/// When no daemon can start (here: its state directory cannot be created),
/// the command runs directly after one warning.
#[test]
fn a_daemon_that_cannot_start_means_a_direct_run() {
    let fixture = Fixture::new("basic");
    let not_a_dir = fixture.path("not-a-dir");
    std::fs::write(&not_a_dir, "").expect("create a file");
    let mut heli = fixture.heli(Vec::<String>::new());
    heli.env("HELI_STATE_DIR", &not_a_dir);
    let warning = assert_ran_directly_with_one_warning(&fixture, &mut heli);
    assert!(warning.contains("state directory"), "{warning}");
}

/// A daemon that hangs up, never answers (heli waits 5 s), or answers as
/// something other than heli means a direct run.
#[test]
fn a_daemon_that_does_not_answer_means_a_direct_run() {
    for (answer, reason) in [
        (Answer::HangUp, "did not answer"),
        (Answer::Never, "did not answer within"),
        (Answer::Bytes(*b"NOPE\x01\0\0\0"), "not heli"),
    ] {
        let fixture = Fixture::new("basic");
        let fake = FakeDaemon::start(&fixture.endpoint(), answer);
        let warning =
            assert_ran_directly_with_one_warning(&fixture, &mut fixture.heli(Vec::<String>::new()));
        assert!(warning.contains(reason), "{answer:?}: {warning}");
        fake.received();
    }
}

/// A daemon of another protocol version means a direct run; an older one is
/// asked to retire once idle, a newer one is left alone.
#[test]
fn a_daemon_of_another_version_means_a_direct_run() {
    for (version, retire) in [(0, true), (u32::MAX, false)] {
        let fixture = Fixture::new("basic");
        let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Version(version));
        let warning =
            assert_ran_directly_with_one_warning(&fixture, &mut fixture.heli(Vec::<String>::new()));
        assert!(
            warning.contains(&format!("protocol {version}")),
            "{warning}"
        );
        let expected: &[u8] = if retire { b"R" } else { b"" };
        assert_eq!(fake.received(), expected, "daemon version {version}");
    }
}

/// A newer client asks the daemon to retire: the daemon takes no new runs
/// (they run directly, after one warning), finishes the runs in flight,
/// then exits, and the next call starts a new daemon.
#[test]
fn a_retiring_daemon_finishes_its_runs_then_exits() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).arg("in flight").env("ECHO_WAIT_FOR", &gate);
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let daemon = support::daemon_pid(&status).expect("a daemon");

    // The handshake every version speaks, from a client of a newer protocol.
    let mut newer = support::endpoint::connect(&fixture.endpoint());
    newer.write_all(b"HELI").expect("send the magic");
    newer
        .write_all(&u32::MAX.to_le_bytes())
        .expect("send the version");
    let mut answer = [0u8; 8];
    newer.read_exact(&mut answer).expect("the daemon's hello");
    assert_eq!(&answer[..4], b"HELI");
    newer.write_all(b"R").expect("ask to retire");
    // The daemon hangs up once it has taken the request.
    let _ = newer.read_to_end(&mut Vec::new());

    let warning =
        assert_ran_directly_with_one_warning(&fixture, &mut fixture.heli(Vec::<String>::new()));
    assert!(warning.contains("shutting down"), "{warning}");
    let status = fixture.status();
    assert_eq!(support::daemon_pid(&status), Some(daemon), "still there");
    assert_eq!(runs(&status).len(), 1, "with its run: {status}");

    open(&gate);
    let output = finish(call);
    assert_success(&output, "the run in flight");
    assert_eq!(text(&output.stdout), "arg: \"in flight\"\n");
    fixture.wait_for_status("the daemon to exit", |status| status["daemon"].is_null());

    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "a later call");
    assert_eq!(
        support::without_footer(&output.stderr),
        "echo-args: stderr\n",
        "no warning"
    );
    let replacement = fixture.daemon_pid().expect("a new daemon");
    assert_ne!(replacement, daemon);
}

/// Only the user can open the daemon's endpoint: on Windows its DACL allows
/// the user alone; on Unix the socket has mode 0600 in a 0700 directory.
#[test]
fn only_the_user_can_open_the_endpoint() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    #[cfg(windows)]
    {
        let entries = support::endpoint::access_entries(&fixture.endpoint());
        assert!(!entries.is_empty(), "someone must be allowed in");
        for entry in &entries {
            assert!(
                entry.allows && entry.current_user,
                "an entry for someone else: {entries:?}"
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        let socket = std::path::PathBuf::from(fixture.endpoint());
        assert_eq!(mode(&socket), 0o600, "the socket");
        assert_eq!(mode(&fixture.state_dir()), 0o700, "the state directory");
    }
}

/// On Windows, a run is in the client's logon session, window station and
/// desktop, so it can reach the user's desktop. On Unix it leads a session
/// of its own (the user's login session reaches it through the
/// environment), so that `cancel` finds every process it started.
#[test]
fn a_run_is_in_the_clients_session() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let direct = fixture
        .command(&echo, Vec::<String>::new())
        .env("ECHO_SESSION", "1")
        .output()
        .expect("run echo-args");
    let heli = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .env("ECHO_SESSION", "1")
        .env("ECHO_PID", "1")
        .output()
        .expect("run heli");
    assert_success(&heli, "cargo heli exec");
    assert!(text(&direct.stdout).starts_with("session "));
    let lines = lines(&heli.stdout);
    #[cfg(windows)]
    assert_eq!(lines[0], text(&direct.stdout).trim_end());
    #[cfg(unix)]
    {
        let pid = lines[1].strip_prefix("pid ").expect("a pid line");
        assert_eq!(lines[0], format!("session {pid}"), "it leads its session");
    }
}

/// The self-run guards hold through the daemon: each nested call sees
/// `HELI_DEPTH` one deeper, the limit stops a nested call, and `heli` is
/// still refused as its own subcommand.
#[test]
fn the_self_run_guards_hold_through_the_daemon() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let heli = support::heli_binary();
    let nested = |depth: Option<&str>| {
        let mut command = fixture.heli(["exec", "--"]);
        command
            .arg(&heli)
            .args(["exec", "--"])
            .arg(&echo)
            .env("ECHO_ENV", "HELI_DEPTH");
        if let Some(depth) = depth {
            command.env("HELI_DEPTH", depth);
        }
        command.output().expect("run heli")
    };

    let output = nested(None);
    assert_success(&output, "nested heli");
    assert_eq!(text(&output.stdout), "env HELI_DEPTH=Some(\"2\")\n");

    let output = nested(Some("15"));
    assert_eq!(output.status.code(), Some(2), "the inner heli refuses");
    assert!(
        text(&output.stderr).contains("nested 16 levels deep"),
        "{}",
        text(&output.stderr)
    );
    assert!(text(&output.stdout).is_empty(), "the command must not run");

    let output = fixture
        .heli(["exec", "--"])
        .arg(&heli)
        .args(["heli", "heli", "exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("would run heli again"),
        "{}",
        text(&output.stderr)
    );
    assert!(text(&output.stdout).is_empty(), "the command must not run");
}

/// A run is over when its process exits, as for a command the client waits
/// for itself: a process it leaves behind, holding its stdout, does not hold
/// the client.
#[test]
fn a_run_is_over_when_its_process_exits() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).arg("parent").env("ECHO_ORPHAN", &gate);
    let call = start(&mut call);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(finish(call)));
    let output = receiver.recv_timeout(std::time::Duration::from_secs(20));
    open(&gate);
    let output = output.expect("heli returned while the orphan still ran");
    assert_success(&output, "cargo heli exec");
    assert_eq!(text(&output.stdout), "arg: \"parent\"\n");
}

/// Without `HELI_STATE_DIR` and `HELI_ENDPOINT`, heli keeps its state under
/// the user's home: `%LOCALAPPDATA%\heli` on Windows, `$XDG_STATE_HOME/heli`
/// or else `~/.local/state/heli` elsewhere. The endpoint follows from the
/// state directory, so two of them have two daemons.
#[test]
fn the_default_state_directory_is_in_the_users_home() {
    // Built with the real home: cargo and rustup live there.
    let builder = Fixture::new("basic");
    let echo = builder.echo_args();
    let cases: &[(bool, &[&str])] = if cfg!(windows) {
        &[(false, &["heli"]), (false, &["heli"])]
    } else {
        &[
            (false, &[".local", "state", "heli"]),
            (true, &["xdg-state", "heli"]),
        ]
    };
    let mut fixtures = Vec::new();
    for &(xdg, state) in cases {
        let fixture = Fixture::with_home("basic", xdg);
        let output = fixture
            .heli(["exec", "--"])
            .arg(&echo)
            .output()
            .expect("run heli");
        assert_success(&output, "cargo heli exec");
        assert_eq!(
            support::without_footer(&output.stderr),
            "echo-args: stderr\n",
            "no warning"
        );
        let state_dir = state
            .iter()
            .fold(fixture.home(), |dir, part| dir.join(part));
        assert!(state_dir.is_dir(), "{} is missing", state_dir.display());
        fixtures.push(fixture);
    }
    let daemons: Vec<Option<u32>> = fixtures.iter().map(Fixture::daemon_pid).collect();
    assert!(daemons[0].is_some(), "status finds the daemon");
    assert_ne!(daemons[0], daemons[1], "one daemon per state directory");
}

/// The daemon does not keep the directory of the call that started it (on
/// Windows that would keep the directory, and the worktree, from being
/// removed).
#[test]
fn the_daemon_does_not_hold_the_callers_directory() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let elsewhere = fixture.path("elsewhere");
    std::fs::create_dir(&elsewhere).expect("create a directory");
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .current_dir(&elsewhere)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    assert!(fixture.daemon_pid().is_some(), "the daemon runs");
    std::fs::remove_dir(&elsewhere).expect("the directory can be removed");
}

/// One daemon per state directory: a second one exits at once, with code 3,
/// and leaves the first one serving.
#[test]
fn a_second_daemon_exits_at_once() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    let first = fixture.daemon_pid().expect("a daemon");

    let mut second = start(&mut fixture.heli(["daemon"]));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = second.try_wait().expect("poll the second daemon") {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    if status.is_none() {
        support::kill_tree(second.id());
    }
    assert_eq!(status.and_then(|status| status.code()), Some(3));
    assert_eq!(fixture.daemon_pid(), Some(first), "the first one serves");
}

/// A call that finds no daemon at its endpoint, while another daemon holds
/// the state directory's lock, waits for that one to listen rather than
/// giving up at once (two calls starting daemons together rely on it). Here
/// the holder listens elsewhere, so after 5 s the call runs directly.
#[test]
fn a_call_waits_for_the_daemon_that_holds_the_lock() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "the first call");
    let mut elsewhere = fixture.endpoint();
    elsewhere.push("-elsewhere");
    let mut heli = fixture.heli(Vec::<String>::new());
    heli.env("HELI_ENDPOINT", &elsewhere);
    let started = std::time::Instant::now();
    let warning = assert_ran_directly_with_one_warning(&fixture, &mut heli);
    assert!(warning.contains("did not listen within 5 s"), "{warning}");
    assert!(started.elapsed() >= std::time::Duration::from_secs(5));
}

/// A run has no input: the client's stdin is not forwarded, and the run
/// reads end-of-file at once instead of waiting.
#[test]
fn a_run_reads_no_input() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo)
        .env("ECHO_STDIN", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut call = call.spawn().expect("start heli");
    call.stdin
        .take()
        .expect("a stdin")
        .write_all(b"not for the run")
        .expect("write stdin");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(finish(call)));
    let output = receiver
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the run did not wait for input");
    assert_success(&output, "cargo heli exec");
    assert_eq!(text(&output.stdout), "stdin 0\n");
}

/// A relative `HELI_STATE_DIR` is relative to the caller's directory, for
/// the daemon too: its files land there.
#[test]
fn a_relative_state_directory_is_the_callers() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .env("HELI_STATE_DIR", "relative-state")
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    let state_dir = fixture.workspace().join("relative-state");
    let files: Vec<_> = std::fs::read_dir(&state_dir)
        .expect("the state directory exists")
        .map(|entry| entry.expect("an entry"))
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .collect();
    assert!(
        !files.is_empty(),
        "the daemon keeps its files in {}",
        state_dir.display()
    );
}

/// The nesting limit also bounds the daemon's own start (it is heli
/// starting heli): a call one level below the limit cannot start a daemon,
/// so it runs the command directly.
#[test]
fn the_nesting_limit_bounds_the_daemons_start() {
    let fixture = Fixture::new("basic");
    let mut heli = fixture.heli(Vec::<String>::new());
    heli.env("HELI_DEPTH", "15");
    let warning = assert_ran_directly_with_one_warning(&fixture, &mut heli);
    assert!(warning.contains("exited with code 2"), "{warning}");
}

/// Arguments and variables that are not valid Unicode reach the run intact.
#[test]
fn arguments_that_are_not_unicode_pass_through() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    #[cfg(windows)]
    let odd = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[u16::from(b'a'), 0xD800, u16::from(b'b')])
    };
    #[cfg(unix)]
    let odd = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'a', 0xFF, b'b'])
    };
    let run = |command: &mut Command| {
        command
            .arg(&odd)
            .env("ECHO_ENV", "HELI_TEST_ODD")
            .env("HELI_TEST_ODD", &odd)
            .output()
            .expect("run")
    };
    let direct = run(&mut fixture.command(&echo, Vec::<String>::new()));
    let heli = run(fixture.heli(["exec", "--"]).arg(&echo));
    assert_success(&heli, "cargo heli exec");
    assert_eq!(text(&heli.stdout), text(&direct.stdout));
    assert_eq!(lines(&heli.stdout).len(), 2, "{}", text(&heli.stdout));
}

/// `cargo heli status` without `--json` prints a line for the daemon and one
/// per run.
#[test]
fn status_prints_text_for_people() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let text_status = || {
        let output = fixture.heli(["status"]).output().expect("run heli status");
        assert_success(&output, "cargo heli status");
        text(&output.stdout)
    };
    let idle = text_status();
    assert!(idle.contains("daemon  not running"), "{idle}");
    assert!(idle.contains("runs    none"), "{idle}");

    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).arg("shown").env("ECHO_WAIT_FOR", &gate);
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let busy = text_status();
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    let pid = support::daemon_pid(&status).expect("a daemon");
    let daemon_line = busy.lines().next().unwrap_or_default();
    let uptime: u64 = daemon_line
        .strip_prefix(&format!("daemon  pid {pid}, protocol "))
        .and_then(|rest| rest.split_once(", up "))
        .and_then(|(_, uptime)| uptime.strip_suffix('s'))
        .and_then(|uptime| uptime.parse().ok())
        .unwrap_or_else(|| panic!("no daemon line: {busy}"));
    assert!(uptime < 120, "{daemon_line}");
    let line = busy
        .lines()
        .find(|line| line.starts_with("runs"))
        .unwrap_or_else(|| panic!("no runs line: {busy}"));
    for part in [id.as_str(), "shown", "running"] {
        assert!(line.contains(part), "{part:?} missing from {line:?}");
    }
    // The run's age, in seconds, ends the line.
    let age: u64 = line
        .strip_suffix('s')
        .and_then(|rest| rest.rsplit(' ').next())
        .and_then(|age| age.parse().ok())
        .unwrap_or_else(|| panic!("no age at the end of {line:?}"));
    assert!(age < 120, "{line:?}");
    open(&gate);
    finish(call);
}

/// A command that exists but cannot be started (a directory) fails as in
/// direct execution: exit 126 and heli's message.
#[test]
fn a_command_that_cannot_start_exits_126() {
    let fixture = Fixture::new("basic");
    let directory = fixture.workspace();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&directory)
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(126), "{}", text(&output.stderr));
    let stderr = lines(&output.stderr);
    assert_eq!(stderr.len(), 1, "one line: {stderr:?}");
    assert!(
        stderr[0].starts_with("cargo-heli: cannot run `"),
        "{stderr:?}"
    );
    assert!(fixture.daemon_pid().is_some(), "the daemon tried it");
}

/// When the daemon dies during a run, the client says so and fails; it does
/// not run the command again itself.
#[test]
fn a_daemon_lost_during_a_run_is_an_error() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).arg("once").env("ECHO_WAIT_FOR", &gate);
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    support::kill_tree(support::daemon_pid(&status).expect("a daemon"));
    let output = finish(call);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stdout),
        "arg: \"once\"\n",
        "what the run printed before, and no second run"
    );
    let stderr = lines(&output.stderr);
    assert_eq!(stderr.len(), 1, "{stderr:?}");
    assert!(
        stderr[0].starts_with("cargo-heli: lost the heli daemon during run r"),
        "{stderr:?}"
    );
}

/// On Unix, a run killed by a signal kills the client with the same signal.
#[cfg(unix)]
#[test]
fn a_run_killed_by_a_signal_ends_the_client_alike() {
    use std::os::unix::process::ExitStatusExt;

    let fixture = Fixture::new("basic");
    let script = ["-c", "kill -TERM $$"];
    let direct = fixture.command("sh", script).output().expect("run sh");
    let heli = fixture
        .heli(["exec", "--", "sh"])
        .args(script)
        .output()
        .expect("run heli");
    assert_eq!(direct.status.signal(), Some(15));
    assert_eq!(heli.status.signal(), direct.status.signal());
}

/// The daemon keeps the environment of the client that started it; a later
/// client's run sees only that client's environment.
#[test]
fn a_run_does_not_see_the_daemons_environment() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let first = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .env("HELI_TEST_LEAK", "from the first client")
        .output()
        .expect("run heli");
    assert_success(&first, "the first run");
    let second = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .env("ECHO_ENV", "HELI_TEST_LEAK")
        .env_remove("HELI_TEST_LEAK")
        .output()
        .expect("run heli");
    assert_success(&second, "the second run");
    assert_eq!(text(&second.stdout), "env HELI_TEST_LEAK=None\n");
}

/// A request the daemon does not know, from a newer client of the same
/// protocol version, gets the answer "unsupported"; asked to, the daemon
/// then retires.
#[test]
fn an_unknown_request_is_answered_as_unsupported() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "a call that starts the daemon");
    assert!(fixture.daemon_pid().is_some(), "a daemon");

    let mut newer = support::endpoint::connect(&fixture.endpoint());
    let mut hello = b"HELI".to_vec();
    hello.extend(support::fake_daemon::PROTOCOL.to_le_bytes());
    newer.write_all(&hello).expect("send the hello");
    let mut answer = [0u8; 8];
    newer.read_exact(&mut answer).expect("the daemon's hello");
    assert_eq!(answer[..], hello[..], "the same protocol version");
    // A message frame: kind 0, a little-endian u32 length, JSON.
    let request = br#"{"FromTheFuture":{"what":1}}"#;
    let mut frame = vec![0u8];
    frame.extend((request.len() as u32).to_le_bytes());
    frame.extend(request);
    newer.write_all(&frame).expect("send the request");
    let mut header = [0u8; 5];
    newer.read_exact(&mut header).expect("the daemon's answer");
    assert_eq!(header[0], 0, "a message");
    let mut reply =
        vec![0u8; u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize];
    newer.read_exact(&mut reply).expect("the daemon's answer");
    assert_eq!(text(&reply), "\"Unsupported\"");
    newer.write_all(b"R").expect("ask to retire");
    let _ = newer.read_to_end(&mut Vec::new());

    fixture.wait_for_status("the daemon to exit", |status| status["daemon"].is_null());
}

/// A daemon that answers "unsupported" is older than the client: the client
/// asks it to retire, and runs the command directly after one warning.
#[test]
fn an_unsupported_answer_means_a_direct_run() {
    let fixture = Fixture::new("basic");
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Unsupported);
    let warning =
        assert_ran_directly_with_one_warning(&fixture, &mut fixture.heli(Vec::<String>::new()));
    assert!(warning.contains("does not support"), "{warning}");
    assert_eq!(fake.received(), b"R", "asked to retire");
}
