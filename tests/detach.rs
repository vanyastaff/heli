//! A run belongs to the daemon, not to its client: a client that is killed
//! or stops reading leaves the run going, and `status` shows who follows
//! each run and how the runs nobody followed to their end ended.

mod support;

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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

/// The run `id` in a `status --json` document, if listed.
fn listed(status: &Value, id: &str) -> Option<Value> {
    runs(status).into_iter().find(|run| run["id"] == id)
}

/// The line `cargo heli status` (text) prints for run `id`.
fn status_line(fixture: &Fixture, id: &str) -> String {
    let output = fixture.heli(["status"]).output().expect("run heli status");
    assert_success(&output, "cargo heli status");
    text(&output.stdout)
        .lines()
        .find(|line| line.split_whitespace().any(|word| word == id))
        .unwrap_or_else(|| panic!("no line for {id}: {}", text(&output.stdout)))
        .to_owned()
}

/// A build whose client is killed mid-build goes on to its end: `status`
/// then lists it as done, with its exit and no client attached, and `log`
/// has its whole output, including what it printed after the client died.
#[test]
fn a_killed_clients_build_goes_on_to_its_end() {
    let fixture = Fixture::new("basic");
    let gate = fixture.path("gate");
    // Full output: cargo's own lines show the build under way.
    let mut build = fixture.heli(["--output", "full", "build", "-p", "gated"]);
    build.env("HELI_FIXTURE_GATE", &gate);
    let client = start(&mut build);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let run = &runs(&status)[0];
    let id = run["id"].as_str().expect("an id").to_owned();
    assert_eq!(run["attached_clients"], 1, "its client: {status}");
    assert_eq!(run["exit"], Value::Null, "{status}");
    wait_until("the build to be under way", || {
        logged_stderr(&fixture, &id).contains("Compiling gated v0.0.0")
    });
    let line = status_line(&fixture, &id);
    assert!(line.contains("  running, 1 attached  "), "{line}");

    support::kill_tree(client.id());
    let _ = finish(client);
    fixture.wait_for_status("the client to be gone", |status| {
        listed(status, &id).is_some_and(|run| run["attached_clients"] == 0)
    });
    std::fs::write(&gate, "").expect("open the gate");
    let status = fixture.wait_for_status("the build to end", |status| {
        listed(status, &id).is_some_and(|run| run["phase"] == "done")
    });
    let run = listed(&status, &id).expect("listed");
    assert_eq!(run["attached_clients"], 0, "{status}");
    assert_eq!(
        run["exit"],
        json!({"code": 0, "signal": null, "cancelled": false}),
        "{status}"
    );
    let started = run["started_at"].as_u64().expect("a start");
    let ended = run["ended_at"].as_u64().expect("an end");
    assert!(started <= ended, "{status}");
    let line = status_line(&fixture, &id);
    assert!(line.contains("  done, exit 0  "), "{line}");

    let stderr = logged_stderr(&fixture, &id);
    for part in ["Compiling gated v0.0.0", "Finished"] {
        assert!(stderr.contains(part), "{part:?} missing from {stderr}");
    }
}

/// What `cargo heli log <run>` prints on stderr.
fn logged_stderr(fixture: &Fixture, run: &str) -> String {
    let logged = fixture.heli(["log", run]).output().expect("run heli log");
    assert_success(&logged, "cargo heli log");
    text(&logged.stderr)
}

/// Starts this fixture's daemon, with a run of `echo-args`.
fn start_daemon(fixture: &Fixture) {
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
}

/// A connection to the daemon that has handed it a run, as a client of
/// this build's protocol would, and then reads nothing more.
struct RawClient {
    #[cfg(unix)]
    _connection: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    _connection: std::fs::File,
    /// The run the daemon started, `r<number>`.
    run: String,
}

/// Asks the daemon to run `argv` in the workspace, with this process's
/// environment plus `env`, and reads its `Started` reply.
fn submit_raw(
    fixture: &Fixture,
    argv: &[&std::ffi::OsStr],
    env: &[(&str, &std::ffi::OsStr)],
) -> RawClient {
    use std::io::{Read, Write};

    let text = |text: &std::ffi::OsStr| text.to_str().expect("Unicode").to_owned();
    let mut variables: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| !env.iter().any(|(set, _)| set == name))
        .collect();
    variables.extend(
        env.iter()
            .map(|(name, value)| ((*name).to_owned(), text(value))),
    );
    let request = json!({"Run": {
        "cwd": text(fixture.workspace().as_os_str()),
        "argv": argv.iter().map(|arg| text(arg)).collect::<Vec<_>>(),
        "env": variables,
    }});
    let request = serde_json::to_vec(&request).expect("JSON");

    let mut connection = support::endpoint::connect(&fixture.endpoint());
    let mut hello = b"HELI".to_vec();
    hello.extend(support::fake_daemon::PROTOCOL.to_le_bytes());
    connection.write_all(&hello).expect("send the hello");
    let mut answer = [0u8; 8];
    connection
        .read_exact(&mut answer)
        .expect("the daemon's hello");
    // A message frame: kind 0, a little-endian u32 length, JSON.
    let mut frame = vec![0u8];
    frame.extend(u32::try_from(request.len()).expect("small").to_le_bytes());
    frame.extend(request);
    connection.write_all(&frame).expect("send the request");
    let mut header = [0u8; 5];
    connection
        .read_exact(&mut header)
        .expect("the daemon's answer");
    let mut reply =
        vec![0u8; u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize];
    connection
        .read_exact(&mut reply)
        .expect("the daemon's answer");
    let reply: Value = serde_json::from_slice(&reply).expect("a JSON reply");
    let run = reply["Started"]["run"]
        .as_str()
        .unwrap_or_else(|| panic!("not started: {reply}"))
        .to_owned();
    RawClient {
        _connection: connection,
        run,
    }
}

/// A client that stops reading holds up neither the run nor its record:
/// the run, which writes far more than any pipe or socket buffer holds,
/// goes on to its end, and its record is whole.
#[test]
fn a_client_that_stops_reading_holds_up_nothing() {
    let fixture = Fixture::new("basic");
    let chatter = fixture.binary("chatter");
    let args = ["100000", "mix"];
    let direct = fixture
        .command(&chatter, args)
        .output()
        .expect("run chatter");
    start_daemon(&fixture);

    let mut argv = vec![chatter.as_os_str()];
    argv.extend(args.iter().map(std::ffi::OsStr::new));
    let client = submit_raw(&fixture, &argv, &[]);
    fixture.wait_for_status("the run to end", |status| {
        listed(status, &client.run).is_none()
    });
    let logged = fixture
        .heli(["log", &client.run])
        .output()
        .expect("run heli log");
    assert_success(&logged, "cargo heli log");
    assert!(logged.stdout == direct.stdout, "stdout differs");
    assert!(logged.stderr == direct.stderr, "stderr differs");
}

/// A completed command does not impose a read-speed deadline on its client
/// while the daemon continues serving. A paused reader can resume the full
/// stream, including its exit code.
#[test]
fn a_slow_client_can_resume_a_completed_runs_output() {
    use std::io::Read;
    let fixture = Fixture::new("basic");
    let chatter = fixture.binary("chatter");
    let args = ["100000", "mix"];
    let direct = fixture
        .command(&chatter, args)
        .output()
        .expect("direct chatter");
    start_daemon(&fixture);
    let mut argv = vec![chatter.as_os_str()];
    argv.extend(args.iter().map(std::ffi::OsStr::new));
    let mut client = submit_raw(&fixture, &argv, &[]);
    fixture.wait_for_status("the noisy run to finish", |status| {
        listed(status, &client.run).is_none()
    });
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let mut header = [0; 5];
        client
            ._connection
            .read_exact(&mut header)
            .expect("a complete output frame or exit code after resuming");
        let mut bytes = vec![0; u32::from_le_bytes(header[1..].try_into().unwrap()) as usize];
        client
            ._connection
            .read_exact(&mut bytes)
            .expect("complete frame payload after resuming");
        match header[0] {
            1 => stdout.extend(bytes),
            2 => stderr.extend(bytes),
            0 => {
                let reply: Value = serde_json::from_slice(&bytes).expect("reply JSON");
                assert_eq!(reply["Exited"]["code"], 0, "{reply}");
                break;
            }
            kind => panic!("unknown frame {kind}"),
        }
    }
    assert!(
        stdout == direct.stdout,
        "stdout must survive a paused reader"
    );
    assert!(
        stderr == direct.stderr,
        "stderr must survive a paused reader"
    );
}

/// When the run log cannot be written (here `runs` in the state directory
/// is a file), a run's client still gets all of its output.
#[test]
fn output_that_cannot_be_recorded_still_reaches_the_client() {
    let fixture = Fixture::new("basic");
    let chatter = fixture.binary("chatter");
    let args = ["40", "mix", "unended"];
    let direct = fixture
        .command(&chatter, args)
        .output()
        .expect("run chatter");
    std::fs::create_dir_all(fixture.state_dir()).expect("create the state directory");
    std::fs::write(fixture.state_dir().join("runs"), "").expect("block the run log");

    let output = fixture
        .heli(["exec", "--"])
        .arg(&chatter)
        .args(args)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    assert!(output.stdout == direct.stdout, "stdout differs");
    let (stderr, _) = support::split_footer(&output.stderr);
    // The footer starts a line of its own.
    let mut expected = direct.stderr.clone();
    expected.push(b'\n');
    assert!(stderr == expected, "stderr differs: {}", text(&stderr));
}

/// `status` keeps listing the last 32 runs that ended while no client
/// followed them; older ones go.
#[test]
fn status_lists_the_last_32_runs_nobody_followed_to_the_end() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    start_daemon(&fixture);
    let ids: Vec<String> = (0..33)
        .map(|number| {
            let number = number.to_string();
            let argv = [echo.as_os_str(), std::ffi::OsStr::new(&number)];
            submit_raw(&fixture, &argv, &[("ECHO_WAIT_FOR", gate.as_os_str())]).run
        })
        .collect();
    fixture.wait_for_status("every client to be gone", |status| {
        let runs = runs(status);
        runs.len() == 33 && runs.iter().all(|run| run["attached_clients"] == 0)
    });

    std::fs::write(&gate, "").expect("open the gate");
    let status = fixture.wait_for_status("the runs to end", |status| {
        let runs = runs(status);
        runs.len() == 32 && runs.iter().all(|run| run["phase"] == "done")
    });
    let listed: Vec<String> = runs(&status)
        .iter()
        .map(|run| run["id"].as_str().expect("an id").to_owned())
        .collect();
    assert_eq!(listed, ids[1..], "{status}");
}

/// Starts `echo-args` through heli (as `cargo heli <args>` with it standing
/// in for cargo, or with no `args` as `exec`), waiting for a gate; kills its
/// client once the run has started, opens the gate once `status` shows no
/// client attached, and returns the run once it is listed as done.
fn run_nobody_follows(fixture: &Fixture, args: &[&str]) -> String {
    let echo = fixture.echo_args();
    let gate = fixture.path("unfollowed.gate");
    let mut call = if args.is_empty() {
        let mut call = fixture.heli(["exec", "--"]);
        call.arg(&echo);
        call
    } else {
        let mut call = fixture.heli(args.iter().copied());
        call.env("CARGO", &echo);
        call
    };
    // It prints its process id before it waits: the sign it has started.
    call.env("ECHO_WAIT_FOR", &gate).env("ECHO_PID", "1");
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    fixture.wait_for_run_output(&id);
    support::kill_tree(call.id());
    let _ = finish(call);
    fixture.wait_for_status("its client to be gone", |status| {
        listed(status, &id).is_some_and(|run| run["attached_clients"] == 0)
    });
    std::fs::write(&gate, "").expect("open the gate");
    fixture.wait_for_status("the run to end", |status| {
        listed(status, &id).is_some_and(|run| run["phase"] == "done")
    });
    id
}

/// A run listed as done is no run in flight: it does not keep a lost slot
/// from coming back once the pool is quiet (the top-up waits only for runs
/// in flight).
#[test]
fn a_run_listed_as_done_does_not_hold_up_a_top_up() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    fixture.set_clock(0);
    fixture.start_daemon();
    run_nobody_follows(&fixture, &[]);
    let lost = support::jobserver::Client::open(&fixture.pool());
    lost.take();
    drop(lost);
    let full = json!({"slots": 2, "free": 2, "withheld": 0, "in_use": 0});
    for available_mb in 40_000..40_010 {
        fixture.inject_memory(support::Memory::available(available_mb));
        fixture.advance_clock(1000);
        let status = fixture.wait_for_status("a sample", |status| {
            status["memory"]["available"] == available_mb << 20
        });
        if status["pool"] == full {
            assert_eq!(runs(&status).len(), 1, "the run is still listed: {status}");
            return;
        }
    }
    panic!("the lost slot never came back: {}", fixture.status());
}

/// A run listed as done holds no worktree lock: a `fmt` of its worktree
/// does not wait for it.
#[test]
fn a_run_listed_as_done_holds_no_worktree_lock() {
    let fixture = Fixture::new("basic");
    run_nobody_follows(&fixture, &["build"]);
    let echo = fixture.echo_args();
    let fmt = start(fixture.heli(["fmt"]).env("CARGO", &echo));
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(finish(fmt)));
    let output = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("fmt waited for a run that had ended");
    assert_success(&output, "cargo heli fmt");
    assert!(
        !text(&output.stderr).contains("waiting for"),
        "{}",
        text(&output.stderr)
    );
}
