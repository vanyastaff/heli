//! `cargo heli rerun` (`DESIGN.md` §5, "Reruns"): a recorded run again,
//! whole or only its failed tests, linked to the run it reruns.
//!
//! Fixture crates: `many-tests` (100 tests, of which `HELI_FIXTURE_FAIL`
//! makes the ones it names fail), `echo-args`.

mod support;

use std::process::{Output, Stdio};

use support::{Fixture, runs};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The two tests `HELI_FIXTURE_FAIL` makes fail.
const FAILING: &str = "g1::t2,g3::t4";

/// `cargo heli <args>`, without the test thread count of the caller's
/// environment, and with `failing` tests of `many-tests` failing.
fn heli(fixture: &Fixture, args: &[&str], failing: Option<&str>) -> Output {
    let mut command = fixture.heli(args);
    command
        .env_remove("NEXTEST_TEST_THREADS")
        .env_remove("RUST_TEST_THREADS");
    match failing {
        Some(failing) => command.env("HELI_FIXTURE_FAIL", failing),
        None => command.env_remove("HELI_FIXTURE_FAIL"),
    };
    command.output().expect("run heli")
}

/// The run the footer of `output` names.
fn run_of(output: &Output) -> String {
    support::split_footer(&output.stderr).1.run
}

/// A test run with two failures out of 100 reruns those two only, as the
/// first line says, linked to it; with `--then-all`, once they pass, the
/// whole run follows.
fn reruns_only_the_failed_tests(command: &[&str], counts: &[&str], all_pass: &str) {
    let fixture = Fixture::new("basic");
    let original = heli(&fixture, command, Some(FAILING));
    assert!(
        !original.status.success(),
        "the run fails: {}",
        text(&original.stderr)
    );
    let of = run_of(&original);

    let rerun = heli(&fixture, &["rerun", "--failed"], Some(FAILING));
    let stderr = text(&rerun.stderr);
    assert!(!rerun.status.success(), "the tests still fail: {stderr}");
    assert_eq!(
        stderr.lines().next(),
        Some(format!("cargo-heli: rerunning 2 failed tests of {of}: g1::t2, g3::t4").as_str()),
        "{stderr}"
    );
    for count in counts {
        assert!(stderr.contains(count), "{count:?} in: {stderr}");
    }
    let again = run_of(&rerun);
    assert_ne!(again, of);
    let log = fixture
        .heli(["log", &again, "--tail", "1"])
        .output()
        .expect("run log");
    assert_eq!(
        text(&log.stderr).lines().next(),
        Some(format!("cargo-heli: {again}: rerun of {of}").as_str()),
        "{}",
        text(&log.stderr)
    );

    let green = heli(&fixture, &["rerun", &of, "--failed", "--then-all"], None);
    let stderr = text(&green.stderr);
    support::assert_success(&green, "cargo heli rerun --failed --then-all");
    assert!(
        stderr.contains(&format!(
            "cargo-heli: the failed tests of {of} pass; rerunning it whole"
        )),
        "{stderr}"
    );
    assert!(stderr.contains(all_pass), "{all_pass:?} in: {stderr}");
}

#[test]
fn libtest_reruns_only_the_failed_tests() {
    reruns_only_the_failed_tests(
        &["--output", "compact", "test", "-p", "many-tests"],
        &["tests: 0 passed, 2 failed, 99 filtered out"],
        "tests: 100 passed, 1 ignored",
    );
}

#[test]
fn nextest_reruns_only_the_failed_tests() {
    support::require_nextest();
    reruns_only_the_failed_tests(
        &[
            "--output",
            "compact",
            "nextest",
            "run",
            "-p",
            "many-tests",
            "--no-fail-fast",
        ],
        &["2 tests run: 0 passed, 2 failed"],
        "100 tests run: 100 passed",
    );
}

/// A rerun runs the recorded command line from the run's directory with
/// the environment it is given, and `status` lists it as a rerun.
#[test]
fn a_whole_rerun_takes_the_current_environment_and_says_what_it_reruns() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let original = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("x")
        .output()
        .expect("run heli");
    support::assert_success(&original, "cargo heli exec");
    let of = run_of(&original);
    let gate = fixture.path("gate");
    let elsewhere = fixture.path("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("a directory");
    let rerun = fixture
        .heli(["rerun", &of])
        .current_dir(&elsewhere)
        .env("ECHO_WAIT_FOR", &gate)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli rerun");
    let status = fixture.wait_for_status("the rerun", |status| {
        runs(status)
            .iter()
            .any(|run| run["rerun_of"].as_str() == Some(&of))
    });
    let listed = runs(&status)
        .into_iter()
        .find(|run| run["rerun_of"].as_str() == Some(&of))
        .expect("the rerun");
    // Canonical: on macOS the temporary directory is reached through a
    // symbolic link (`/var` is `/private/var`).
    let canonical = |path: std::path::PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    assert_eq!(
        canonical(listed["worktree"].as_str().expect("a worktree").into()),
        canonical(fixture.workspace())
    );
    std::fs::write(&gate, "").expect("open the gate");
    let rerun = rerun.wait_with_output().expect("wait for heli rerun");
    support::assert_success(&rerun, "cargo heli rerun");
    let stderr = text(&rerun.stderr);
    assert!(
        stderr.starts_with(&format!("cargo-heli: rerunning {of}: cargo heli exec -- ")),
        "{stderr}"
    );
    assert_eq!(text(&rerun.stdout), text(&original.stdout));
}

/// What cannot be rerun is one line, and exit 1.
#[test]
fn what_cannot_be_rerun_is_one_line_and_exit_1() {
    let fixture = Fixture::new("basic");
    let refused = |args: &[&str], reason: &str| {
        let output = heli(&fixture, args, None);
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert_eq!(stderr.lines().count(), 1, "{args:?}: {stderr}");
        assert!(stderr.contains(reason), "{args:?}: {reason:?} in {stderr}");
    };
    refused(&["rerun", "--failed"], "no test run with failed tests");
    refused(&["rerun", "r999", "--failed"], "no run r999 in the run log");

    let exec = fixture
        .heli(["exec", "--"])
        .arg(fixture.echo_args())
        .output()
        .expect("run heli");
    let exec = run_of(&exec);
    refused(&["rerun", &exec, "--failed"], "is not a test run");

    let passed = heli(&fixture, &["test", "-p", "many-tests", "--lib"], None);
    support::assert_success(&passed, "cargo heli test");
    let passed = run_of(&passed);
    refused(
        &["rerun", &passed, "--failed"],
        "has no failed tests to rerun",
    );
}

/// `--then-all` only goes with `--failed`.
#[test]
fn then_all_needs_failed() {
    let fixture = Fixture::new("basic");
    let output = fixture
        .heli(["rerun", "--then-all"])
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
}
