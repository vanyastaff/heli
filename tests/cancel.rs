//! `cargo heli cancel <run>` kills the run's whole process tree, however
//! deep and in whatever process groups, and the run is reported as
//! cancelled.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::fake_daemon::{Answer, FakeDaemon};
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

/// Polls `done` until it holds; panics after 120 s, naming `what` it waited
/// for.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A file the fixture processes wait for; created when dropped, so that
/// whatever a failed test leaves behind ends too.
struct Gate(PathBuf);

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

/// `cargo heli cancel` with `args`.
fn cancel(fixture: &Fixture, run: &str) -> Output {
    fixture
        .heli(["cancel", run])
        .output()
        .expect("run heli cancel")
}

/// A process the test started indirectly, watched by its id; on Windows
/// through a handle opened while it runs, so a reused id cannot pass for
/// it.
struct Watched {
    pid: u32,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

impl Watched {
    /// Watches process `pid`, which must be running.
    fn open(pid: u32) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawHandle;
            use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
            // SAFETY: opens a handle this value owns from here on.
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            assert!(!handle.is_null(), "process {pid} is not running");
            Self {
                pid,
                // SAFETY: a fresh process handle.
                handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
            }
        }
        #[cfg(unix)]
        {
            let watched = Self { pid };
            assert!(watched.alive(), "process {pid} is not running");
            watched
        }
    }

    /// Whether it still runs.
    fn alive(&self) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            // SAFETY: a process handle with SYNCHRONIZE access.
            unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) == WAIT_TIMEOUT }
        }
        #[cfg(unix)]
        {
            // `ps` prints nothing for a process that is gone, and `Z` for
            // one that is dead but not yet reaped.
            let output = Command::new("ps")
                .args(["-o", "stat=", "-p", &self.pid.to_string()])
                .output()
                .expect("run ps");
            let state = text(&output.stdout);
            let state = state.trim();
            !state.is_empty() && !state.starts_with('Z')
        }
    }
}

/// The processes that wrote their ids into `dir`, once there are `count`.
fn watch(dir: &Path, count: usize) -> Vec<Watched> {
    let ids = || -> Vec<u32> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    wait_until(&format!("{count} processes"), || ids().len() == count);
    ids().into_iter().map(Watched::open).collect()
}

/// The only run `status` lists.
fn only_run(fixture: &Fixture) -> String {
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    runs(&status)[0]["id"].as_str().expect("an id").to_owned()
}

/// Asserts that `output` is a client's whose run `id` was cancelled.
fn assert_cancelled(output: &Output, id: &str) {
    assert!(!output.status.success(), "{output:?}");
    #[cfg(windows)]
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(output.status.signal(), Some(15), "{output:?}");
    }
    let (_, footer) = support::split_footer(&output.stderr);
    assert_eq!(footer.run, id);
    assert!(
        footer.line.contains(&format!("{id} cancelled, ")),
        "{}",
        footer.line
    );
}

/// `cancel` kills every process of the run, a child in a process group of
/// its own too (as nextest starts each test), and returns once they are
/// gone; the run's client reports the run as cancelled.
#[test]
fn cancel_kills_every_process_of_the_run() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let _gate = Gate(fixture.path("gate"));
    let tree = fixture.path("tree");
    std::fs::create_dir(&tree).expect("create the tree directory");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo)
        .arg("doomed")
        .env("ECHO_TREE", &tree)
        .env("ECHO_WAIT_FOR", fixture.path("gate"));
    let call = start(&mut call);
    let processes = watch(&tree, 2);
    let id = only_run(&fixture);

    let cancelled = cancel(&fixture, &id);
    assert_success(&cancelled, "cargo heli cancel");
    assert_eq!(
        text(&cancelled.stderr),
        format!("cargo-heli: cancelled {id}\n")
    );
    for process in &processes {
        assert!(!process.alive(), "process {} survived", process.pid);
    }
    let output = finish(call);
    assert_cancelled(&output, &id);
    assert_eq!(text(&output.stdout), "arg: \"doomed\"\n");
    assert!(fixture.runs().is_empty(), "its client was told");
}

/// `cancel` stops a `cargo test` with its test processes, among them one
/// in a process group of its own.
#[test]
fn cancel_stops_a_cargo_test_and_its_test_processes() {
    let fixture = Fixture::new("basic");
    let _gate = Gate(fixture.path("gate"));
    let pids = fixture.path("pids");
    std::fs::create_dir(&pids).expect("create the pids directory");
    let mut test = fixture.heli(["test", "-p", "linger"]);
    test.env("HELI_FIXTURE_PIDS", &pids)
        .env("HELI_FIXTURE_GATE", fixture.path("gate"));
    let call = start(&mut test);
    let processes = watch(&pids, 2);
    let id = only_run(&fixture);

    assert_success(&cancel(&fixture, &id), "cargo heli cancel");
    for process in &processes {
        assert!(!process.alive(), "test process {} survived", process.pid);
    }
    assert_cancelled(&finish(call), &id);
}

/// A run cancelled while no client follows it is listed as done and
/// cancelled. Cancelling it again, or a run that never was, fails.
#[test]
fn a_cancelled_run_is_listed_as_cancelled() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let _gate = Gate(fixture.path("gate"));
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).env("ECHO_WAIT_FOR", fixture.path("gate"));
    let call = start(&mut call);
    let id = only_run(&fixture);
    support::kill_tree(call.id());
    let _ = finish(call);
    fixture.wait_for_status("the client to be gone", |status| {
        runs(status)[0]["attached_clients"] == 0
    });

    assert_success(&cancel(&fixture, &id), "cargo heli cancel");
    let status = fixture.wait_for_status("the run to end", |status| {
        runs(status)[0]["phase"] == "done"
    });
    let exit = &runs(&status)[0]["exit"];
    assert_eq!(exit["cancelled"], true, "{status}");

    let again = cancel(&fixture, &id);
    assert_eq!(again.status.code(), Some(1), "{again:?}");
    let stderr = text(&again.stderr);
    assert!(
        stderr.starts_with(&format!("cargo-heli: {id} has already ended: cancelled, ")),
        "{stderr}"
    );
    let unknown = cancel(&fixture, "r999");
    assert_eq!(unknown.status.code(), Some(1), "{unknown:?}");
    assert_eq!(
        text(&unknown.stderr),
        "cargo-heli: no run r999 is in flight\n"
    );
}

/// A run cancelled while it waits for its worktree lock stops waiting and
/// never starts: its client reports it cancelled and exits 1, and the run
/// it waited for goes on.
#[test]
fn a_run_cancelled_while_it_waits_never_starts() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let _gate = Gate(fixture.path("gate"));
    let starts = fixture.path("starts");
    // `echo-args` stands in for cargo: a build, held by the gate, and a
    // `fmt`, which waits for the build.
    let mut build = fixture.heli(["build"]);
    build
        .env("CARGO", &echo)
        .env("ECHO_WAIT_FOR", fixture.path("gate"));
    let build = start(&mut build);
    fixture.wait_for_status("the build", |status| runs(status).len() == 1);
    let mut fmt = fixture.heli(["fmt"]);
    fmt.env("CARGO", &echo).env("ECHO_STARTS", &starts);
    let fmt = start(&mut fmt);
    let status = fixture.wait_for_status("fmt to wait", |status| {
        runs(status).len() == 2 && runs(status)[1]["waiting_since"].is_u64()
    });
    let id = runs(&status)[1]["id"].as_str().expect("an id").to_owned();

    assert_success(&cancel(&fixture, &id), "cargo heli cancel");
    let output = finish(fmt);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let (_, footer) = support::split_footer(&output.stderr);
    assert!(
        footer.line.contains(&format!("{id} cancelled, exit 1 ")),
        "{}",
        footer.line
    );
    assert!(!starts.exists(), "fmt never started");
    let status = fixture.status();
    assert_eq!(runs(&status).len(), 1, "the build goes on: {status}");
    drop(_gate);
    assert_success(&finish(build), "the build");
}

/// Without a daemon there is nothing to cancel: `cancel` says so, fails,
/// and starts no daemon. A run id that is not one is a usage error.
#[test]
fn cancel_without_a_daemon_fails() {
    let fixture = Fixture::new("basic");
    let output = cancel(&fixture, "r1");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        text(&output.stderr),
        "cargo-heli: no heli daemon is running, so no run is in flight\n"
    );
    assert_eq!(fixture.daemon_pid(), None, "cancel started no daemon");

    let output = cancel(&fixture, "banana");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        text(&output.stderr).contains("a run id is r<number>"),
        "{}",
        text(&output.stderr)
    );
}

/// A daemon that does not support `cancel` (an older one) is asked to
/// retire, and `cancel` fails with one line saying why.
#[test]
fn cancel_asks_an_older_daemon_to_retire() {
    let fixture = Fixture::new("basic");
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Unsupported);
    let output = cancel(&fixture, "r1");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("cargo-heli: cannot cancel r1: ")
            && stderr.contains("does not support")
            && stderr.lines().count() == 1,
        "{stderr}"
    );
    assert_eq!(fake.received(), b"R", "asked to retire");
}
