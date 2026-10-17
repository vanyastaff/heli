//! Test runs (DESIGN.md §5, lifecycle step 6): `cargo heli test` and
//! `cargo heli nextest run` build first, then take test slots from the pool
//! and run the tests with as many threads.
//!
//! Some tests run a stand-in for cargo: `echo-args` copied as `cargo`, so
//! heli takes it for cargo, and each invocation prints its arguments.

mod support;

use std::path::PathBuf;
use std::process::Output;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use support::{ConcurrencyLog, Fixture, assert_success, lines, runs};

#[test]
fn missing_nextest_fails_before_waiting_for_test_slots() {
    support::assert_nextest_required("test_threads_one_means_one_slot");
}

/// `echo-args` copied into the fixture's directory as `cargo`.
fn fake_cargo(fixture: &Fixture) -> PathBuf {
    let echo = fixture.echo_args();
    let dir = fixture.path("fake");
    std::fs::create_dir_all(&dir).expect("create the fake cargo's directory");
    let cargo = dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    support::copy_program(&echo, &cargo);
    cargo
}

/// `cargo heli <args>` run with the fake cargo; it must succeed. In full
/// output, unless `configure` chooses another mode, so that heli adds
/// nothing to the arguments the fake cargo prints (compact output adds the
/// flags of its views; `tests/output.rs`, `tests/test_output.rs`).
fn with_fake_cargo(
    fixture: &Fixture,
    args: &[&str],
    configure: impl Fn(&mut std::process::Command),
) -> Output {
    let cargo = fake_cargo(fixture);
    let mut heli = fixture.heli(args);
    heli.env("CARGO", &cargo)
        .env("HELI_OUTPUT", "full")
        .env_remove("RUST_TEST_THREADS")
        .env_remove("NEXTEST_TEST_THREADS");
    configure(&mut heli);
    let output = heli.output().expect("run heli");
    assert_success(&output, &format!("cargo heli {}", args.join(" ")));
    output
}

/// The arguments the fake cargo printed, one line each.
fn printed_args(output: &Output) -> Vec<String> {
    lines(&output.stdout)
}

/// heli adds no job count: a build passes the user's `-j` and
/// `CARGO_BUILD_JOBS` through unchanged, and adds neither when the user
/// gives none. In full output the arguments are exactly the user's; in
/// compact output (the default without a terminal) heli adds only its
/// output flags.
#[test]
fn heli_adds_no_job_count() {
    let fixture = Fixture::new("basic");
    let output = with_fake_cargo(&fixture, &["--output=full", "build", "-j", "3"], |heli| {
        heli.env("ECHO_ENV", "CARGO_BUILD_JOBS")
            .env("CARGO_BUILD_JOBS", "5");
    });
    assert_eq!(
        printed_args(&output),
        [
            "arg: \"build\"",
            "arg: \"-j\"",
            "arg: \"3\"",
            "env CARGO_BUILD_JOBS=Some(\"5\")"
        ]
    );
    let output = with_fake_cargo(&fixture, &["--output=full", "build"], |heli| {
        heli.env("ECHO_ENV", "CARGO_BUILD_JOBS");
    });
    assert_eq!(
        printed_args(&output),
        ["arg: \"build\"", "env CARGO_BUILD_JOBS=None"]
    );
    let output = with_fake_cargo(&fixture, &["build"], |heli| {
        heli.env("ECHO_ENV", "CARGO_BUILD_JOBS")
            .env_remove("HELI_OUTPUT");
    });
    let printed = printed_args(&output);
    assert!(
        printed.contains(&"env CARGO_BUILD_JOBS=None".to_owned())
            && !printed
                .iter()
                .any(|arg| arg.starts_with("arg: \"-j") || arg.starts_with("arg: \"--jobs")),
        "{printed:?}"
    );
}

/// `cargo heli test` first builds with `--no-run`, then runs the tests with
/// `--test-threads=K` for libtest, after the user's own arguments; K is the
/// pool's 2 slots for a run alone. The user's `-j` stays as it was.
#[test]
fn a_test_run_builds_then_tests() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let output = with_fake_cargo(&fixture, &["test", "-j", "3", "--", "--nocapture"], |_| {});
    assert_eq!(
        printed_args(&output),
        [
            "arg: \"test\"",
            "arg: \"--no-run\"",
            "arg: \"-j\"",
            "arg: \"3\"",
            "arg: \"--\"",
            "arg: \"--nocapture\"",
            "arg: \"test\"",
            "arg: \"-j\"",
            "arg: \"3\"",
            "arg: \"--\"",
            "arg: \"--nocapture\"",
            "arg: \"--test-threads=2\"",
        ]
    );
}

/// A user's `--test-threads` bounds K: `cargo heli nextest run -j 1` tests
/// with `--test-threads 1` in place of the user's flag, though the pool has
/// 2 slots free. The build keeps the user's other arguments (nextest
/// ignores a thread count while it builds, with a warning).
#[test]
fn the_threads_asked_for_bound_the_test_slots() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let output = with_fake_cargo(
        &fixture,
        &["nextest", "run", "-j", "1", "-p", "tests-mixed"],
        |_| {},
    );
    assert_eq!(
        printed_args(&output),
        [
            "arg: \"nextest\"",
            "arg: \"run\"",
            "arg: \"--no-run\"",
            "arg: \"-p\"",
            "arg: \"tests-mixed\"",
            "arg: \"nextest\"",
            "arg: \"run\"",
            "arg: \"--test-threads\"",
            "arg: \"1\"",
            "arg: \"-p\"",
            "arg: \"tests-mixed\"",
        ]
    );
    let output = with_fake_cargo(&fixture, &["test", "--", "--test-threads", "1"], |_| {});
    assert_eq!(
        printed_args(&output)[5..],
        ["arg: \"test\"", "arg: \"--\"", "arg: \"--test-threads=1\""]
    );
}

/// A test run through heli in the background, whose `test-slots` tests log
/// to a concurrency log and wait for a gate.
struct Gated {
    child: Child,
    gate: PathBuf,
    log: ConcurrencyLog,
}

impl Gated {
    /// Starts `cargo heli <args>`.
    fn start(fixture: &Fixture, args: &[&str]) -> Self {
        let gate = fixture.path("gate");
        let log = fixture.concurrency_log_named("tests");
        let mut heli = fixture.heli(args);
        heli.env("HELI_FIXTURE_GATE", &gate)
            .env_remove("RUST_TEST_THREADS")
            .env_remove("NEXTEST_TEST_THREADS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        log.attach(&mut heli, 0);
        let child = heli.spawn().expect("start heli");
        Self { child, gate, log }
    }

    /// Waits until `tests` tests run at once (they wait for the gate), and
    /// checks that no more start meanwhile.
    fn wait_running(&self, tests: usize) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.log.running() < tests {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {tests} tests to run"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(self.log.running(), tests, "no more tests start");
    }

    /// Opens the gate and waits for the run, which must pass. Returns its
    /// output and the most tests that ran at once.
    fn finish(self) -> (Output, usize) {
        std::fs::write(&self.gate, "").expect("open the gate");
        let output = self.child.wait_with_output().expect("wait for heli");
        assert_success(&output, "the test run");
        assert_eq!(self.log.intervals().len(), 4, "every test ran");
        (output, self.log.max_concurrency())
    }
}

/// The header line of a run's test phase, from its stderr.
fn header(output: &Output) -> String {
    lines(&output.stderr)
        .into_iter()
        .find(|line| line.starts_with("cargo-heli: r") && line.contains(": testing on "))
        .unwrap_or_else(|| {
            panic!(
                "no test header: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

/// The seconds a test header says the run waited for its slots.
fn waited(header: &str) -> f64 {
    header
        .rsplit_once("waited ")
        .and_then(|(_, rest)| rest.strip_suffix(" s"))
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or_else(|| panic!("no wait in {header:?}"))
}

/// The only run in `status`.
fn only_run(status: &Value) -> Value {
    let runs = runs(status);
    assert_eq!(runs.len(), 1, "{status}");
    runs[0].clone()
}

/// `cargo heli test` (libtest) takes K test slots, here the pool's 2, runs
/// its tests on as many threads, and holds the slots in `status` until the
/// tests end; the header says so.
#[test]
fn libtest_holds_its_test_slots_until_the_tests_end() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let tests = Gated::start(&fixture, &["test", "-p", "test-slots"]);
    tests.wait_running(2);
    let run = only_run(&fixture.status());
    assert_eq!(run["phase"], "testing", "{run}");
    assert_eq!(run["slots"], 2, "{run}");
    assert_eq!(run["share"].as_f64(), Some(2.0), "{run}");
    let (output, most) = tests.finish();
    assert_eq!(most, 2);
    let header = header(&output);
    assert!(
        header.contains(": testing on 2 threads, 2 slots (share 2), waited "),
        "{header}"
    );
    let status = fixture.status();
    assert!(runs(&status).is_empty(), "{status}");
    assert_eq!(status["pool"]["free"], 2, "{status}");
}

/// `--test-threads 1` from the user means K = 1: nextest runs one test at a
/// time on 1 slot, though the pool has 2.
#[test]
fn test_threads_one_means_one_slot() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let tests = Gated::start(
        &fixture,
        &["nextest", "run", "-p", "test-slots", "--test-threads", "1"],
    );
    tests.wait_running(1);
    let run = only_run(&fixture.status());
    assert_eq!(run["phase"], "testing", "{run}");
    assert_eq!(run["slots"], 1, "{run}");
    let (output, most) = tests.finish();
    assert_eq!(most, 1);
    let header = header(&output);
    assert!(
        header.contains(": testing on 1 thread, 1 slot (share 2), "),
        "{header}"
    );
}

/// A process that holds every slot of the fixture's host pool, as bare
/// cargo would, until it is dropped.
struct Saturated(Child);

impl Saturated {
    /// Starts the fixture's daemon, then a `jobs` that takes all `slots`
    /// slots of its pool directly, and waits until none is free. The
    /// governor's clock stands still, so no top-up counts the slots `jobs`
    /// holds as lost (it is no process the top-up's scan sees).
    fn pool(fixture: &Fixture, slots: u32) -> Self {
        fixture.set_clock(0);
        fixture.start_daemon();
        let holder = fixture
            .command(fixture.binary("jobs"), Vec::<String>::new())
            .env("CARGO_MAKEFLAGS", fixture.makeflags())
            .env("JOBS_WORKERS", slots.to_string())
            .env("JOBS_HOLD_MS", "600000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start jobs");
        let saturated = Self(holder);
        fixture.wait_for_status("every slot held", |status| status["pool"]["free"] == 0);
        saturated
    }
}

impl Drop for Saturated {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// With no slot free for 5 s, the tests run on one thread and hold no pool
/// slot: the run waits 5 s, then runs one test at a time with 0 slots.
#[test]
fn with_no_slot_free_the_tests_run_on_one_thread() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let _saturated = Saturated::pool(&fixture, 2);
    let tests = Gated::start(&fixture, &["nextest", "run", "-p", "test-slots"]);
    tests.wait_running(1);
    let run = only_run(&fixture.status());
    assert_eq!(run["phase"], "testing", "{run}");
    assert_eq!(run["slots"], 0, "{run}");
    let (output, most) = tests.finish();
    assert_eq!(most, 1);
    let header = header(&output);
    assert!(
        header.contains(": testing on 1 thread, no pool slot free"),
        "{header}"
    );
    assert!(waited(&header) >= 5.0, "{header}");
}

/// The wait for test slots comes before nextest starts, so it never counts
/// toward a slow-timeout: with the pool saturated and tests killed after
/// 2 s, a run that waits 5 s for slots still passes.
#[test]
fn the_wait_for_test_slots_is_not_test_time() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 1\n");
    let config = fixture.workspace().join(".config");
    std::fs::create_dir_all(&config).expect("create .config");
    std::fs::write(
        config.join("nextest.toml"),
        "[profile.default]\nslow-timeout = { period = \"1s\", terminate-after = 2 }\n",
    )
    .expect("write the nextest configuration");
    let _saturated = Saturated::pool(&fixture, 1);
    let output = fixture
        .heli([
            "nextest",
            "run",
            "-p",
            "tests-mixed",
            "-E",
            "test(=tests::passes)",
        ])
        .env_remove("NEXTEST_TEST_THREADS")
        .output()
        .expect("run heli");
    assert_success(&output, "the test run");
    let header = header(&output);
    assert!(waited(&header) >= 5.0, "{header}");
}

/// A nested cargo build inside a test draws from the run's own pool, through
/// the environment it inherits: the nested project's build script gets the
/// run pool, `<host pool>-<run id>`, as its jobserver.
#[test]
fn a_nested_build_draws_from_the_run_pool() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let seen = fixture.path("nested-makeflags");
    let output = fixture
        .heli(["nextest", "run", "-p", "nested"])
        .env("HELI_FIXTURE_MAKEFLAGS_LOG", &seen)
        .env_remove("NEXTEST_TEST_THREADS")
        .output()
        .expect("run heli");
    assert_success(&output, "the test run");
    let header = header(&output);
    let id = header
        .strip_prefix("cargo-heli: ")
        .and_then(|rest| rest.split_once(':'))
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("no run id in {header:?}"));
    let pool = fixture.pool().into_string().expect("a Unicode pool name");
    let auth = if cfg!(windows) {
        format!("{pool}-{id}")
    } else {
        format!("fifo:{pool}-{id}")
    };
    let makeflags = std::fs::read_to_string(&seen).expect("the nested build script ran");
    assert!(
        makeflags
            .split(' ')
            .any(|flag| flag == format!("--jobserver-auth={auth}")),
        "{makeflags}"
    );
}

/// While a test run builds, `status` shows it `building`; a build that
/// fails ends the run with the build's exit code, and runs no tests.
#[test]
fn a_test_run_shows_its_build_and_stops_when_it_fails() {
    let fixture = Fixture::new("basic");
    let cargo = fake_cargo(&fixture);
    let gate = fixture.path("gate");
    let building = fixture
        .heli(["--output=full", "test"])
        .env("CARGO", &cargo)
        .env("ECHO_WAIT_FOR", &gate)
        .env("ECHO_EXIT", "3")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli");
    // A run is admitted as `running` and becomes `building` once its build
    // phase starts; the gated build cannot get past `building` meanwhile.
    let status = fixture.wait_for_status("the run building", |status| {
        runs(status).len() == 1 && only_run(status)["phase"] == "building"
    });
    assert_eq!(only_run(&status)["phase"], "building", "{status}");
    std::fs::write(&gate, "").expect("open the gate");
    let output = building.wait_with_output().expect("wait for heli");
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert_eq!(
        printed_args(&output),
        ["arg: \"test\"", "arg: \"--no-run\""]
    );
}

/// Only runs of class test (`cargo heli test`, `cargo heli nextest run`)
/// are test runs, not `cargo heli exec -- cargo test`, and only when they
/// can build first: `--no-run` is a build, and `cargo test --doc` (which
/// cargo cannot build apart) runs its tests at once, on its test slots. A
/// `+toolchain` before the subcommand stays where it is.
#[test]
fn what_is_a_test_run() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let output = with_fake_cargo(&fixture, &["test", "--no-run"], |_| {});
    assert_eq!(
        printed_args(&output),
        ["arg: \"test\"", "arg: \"--no-run\""]
    );
    let output = with_fake_cargo(&fixture, &["test", "--doc"], |_| {});
    assert_eq!(
        printed_args(&output),
        [
            "arg: \"test\"",
            "arg: \"--doc\"",
            "arg: \"--\"",
            "arg: \"--test-threads=2\""
        ]
    );
    let output = with_fake_cargo(&fixture, &["+stable", "nextest", "r"], |_| {});
    assert_eq!(
        printed_args(&output),
        [
            "arg: \"+stable\"",
            "arg: \"nextest\"",
            "arg: \"r\"",
            "arg: \"--no-run\"",
            "arg: \"+stable\"",
            "arg: \"nextest\"",
            "arg: \"r\"",
            "arg: \"--test-threads\"",
            "arg: \"2\"",
        ]
    );
    let cargo = fake_cargo(&fixture);
    let output = fixture
        .heli(["exec", "--"])
        .arg(&cargo)
        .arg("test")
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec -- cargo test");
    assert_eq!(printed_args(&output), ["arg: \"test\""], "class exec");
}

/// The threads asked for come from every place the tools read them:
/// nextest's `-j<n>` and `--test-threads=<n>` and `NEXTEST_TEST_THREADS`,
/// libtest's `--test-threads=<n>` and `RUST_TEST_THREADS`.
#[test]
fn the_threads_asked_for_come_from_flags_and_the_environment() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let test_threads = |output: &Output| -> Vec<String> {
        let args = printed_args(output);
        let tests = args
            .iter()
            .rposition(|arg| arg == "arg: \"nextest\"" || arg == "arg: \"test\"")
            .expect("a test invocation");
        args[tests..].to_vec()
    };
    for args in [
        &["nextest", "run", "-j1"][..],
        &["nextest", "run", "--test-threads=1"],
        &["nextest", "run", "--jobs", "1"],
    ] {
        let output = with_fake_cargo(&fixture, args, |_| {});
        assert_eq!(
            test_threads(&output)[2..],
            ["arg: \"--test-threads\"", "arg: \"1\""],
            "{args:?}"
        );
    }
    let output = with_fake_cargo(&fixture, &["nextest", "run"], |heli| {
        heli.env("NEXTEST_TEST_THREADS", "1");
    });
    assert_eq!(
        test_threads(&output)[2..],
        ["arg: \"--test-threads\"", "arg: \"1\""]
    );
    let output = with_fake_cargo(&fixture, &["test", "--", "--test-threads=1"], |_| {});
    assert_eq!(
        test_threads(&output),
        ["arg: \"test\"", "arg: \"--\"", "arg: \"--test-threads=1\""]
    );
    let output = with_fake_cargo(&fixture, &["test"], |heli| {
        heli.env("RUST_TEST_THREADS", "1");
    });
    assert_eq!(
        test_threads(&output),
        ["arg: \"test\"", "arg: \"--\"", "arg: \"--test-threads=1\""]
    );
}

/// A test run asks for no more than its share: with N = 4 held by a busy
/// run, a test run's share is 2, and it gets 2 slots back from the busy run
/// well within the 5 s it may wait.
#[test]
fn a_test_run_asks_for_its_share() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = fixture.binary("jobs");
    let stop = fixture.path("busy.stop");
    let busy = fixture
        .heli(["exec", "--"])
        .arg(&jobs)
        .env("JOBS_WORKERS", "8")
        .env("JOBS_STOP", &stop)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli");
    fixture.wait_for_status("the busy run to hold every slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 4
    });
    let output = with_fake_cargo(&fixture, &["test"], |_| {});
    std::fs::write(&stop, "").expect("stop the busy run");
    assert_success(&busy.wait_with_output().expect("wait"), "the busy run");
    assert_eq!(
        printed_args(&output)[2..],
        ["arg: \"test\"", "arg: \"--\"", "arg: \"--test-threads=2\""]
    );
    let header = header(&output);
    assert!(header.contains(" 2 slots (share 2)"), "{header}");
    assert!(waited(&header) < 4.0, "{header}");
}
