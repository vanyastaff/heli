//! Output of test runs and of `exec` (docs/runs.md, "Output"): what the
//! client prints of `cargo heli nextest run`, `cargo heli test` and
//! `cargo heli exec` in compact and quiet mode, and
//! `cargo heli log <run> --test <name>`.
//!
//! Fixture crates: `many-tests` (100 passing tests), `tests-mixed` (a test
//! that passes, one that fails with captured output, a slow one).

mod support;

use std::process::Output;

use support::Fixture;

#[test]
fn missing_nextest_explains_the_output_test_prerequisite() {
    support::assert_nextest_required("a_quiet_green_nextest_run_prints_one_line_with_counts");
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The green part of the fixture's tests: `many-tests` and `tests-mixed`
/// without its failing test, as nextest selects them.
const GREEN_NEXTEST: [&str; 8] = [
    "nextest",
    "run",
    "-p",
    "many-tests",
    "-p",
    "tests-mixed",
    "-E",
    "not test(=tests::fails)",
];

/// `cargo heli <args>`, without the test thread count of the caller's
/// environment.
fn heli(fixture: &Fixture, args: &[&str]) -> Output {
    fixture
        .heli(args)
        .env_remove("NEXTEST_TEST_THREADS")
        .env_remove("RUST_TEST_THREADS")
        .output()
        .expect("run heli")
}

/// Compact output of a green nextest run is at least 80 % smaller than full
/// output, in bytes: nextest prints no line per passing test, and cargo no
/// line per unit. Full output is measured second, on a warm build, when it
/// is smallest.
#[test]
fn compact_output_of_a_green_nextest_run_is_at_least_80_percent_smaller() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let size = |mode: &str| {
        let args: Vec<&str> = ["--output", mode]
            .into_iter()
            .chain(GREEN_NEXTEST)
            .collect();
        let output = heli(&fixture, &args);
        support::assert_success(&output, &format!("cargo heli {args:?}"));
        output.stdout.len() + output.stderr.len()
    };
    let compact = size("compact");
    let full = size("full");
    assert!(
        compact * 5 <= full,
        "compact {compact} bytes, full {full} bytes: {:.0} % smaller",
        100.0 - 100.0 * compact as f64 / full as f64
    );
}

/// The run that quiet mode's line `ok  <command>: <facts><seconds> s
/// [<run>]` names, after checking that the whole output is that one line,
/// on stderr.
fn quiet_line(output: &Output, command: &str, facts: &str) -> String {
    support::assert_success(output, &format!("cargo heli -q {command}"));
    assert_eq!(text(&output.stdout), "", "nothing on stdout");
    let stderr = text(&output.stderr);
    let (seconds, run) = stderr
        .strip_prefix(&format!("ok  {command}: {facts}"))
        .and_then(|rest| rest.strip_suffix("]\n"))
        .and_then(|rest| rest.split_once(" s  ["))
        .unwrap_or_else(|| panic!("not one quiet line: {stderr:?}"));
    let seconds: f64 = seconds.parse().unwrap_or_else(|_| panic!("{stderr:?}"));
    assert!((0.0..300.0).contains(&seconds), "{stderr:?}");
    run.to_owned()
}

/// In quiet mode, a green nextest run prints one line: the counts of
/// nextest's summary (passed, skipped), the duration and the run.
#[test]
fn a_quiet_green_nextest_run_prints_one_line_with_counts() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let args: Vec<&str> = ["-q"].into_iter().chain(GREEN_NEXTEST).collect();
    let output = heli(&fixture, &args);
    quiet_line(&output, &GREEN_NEXTEST.join(" "), "102 passed, 2 skipped, ");
}

/// In compact mode, a failing nextest test shows its name, its panic message
/// and its captured output, and no passing test has a line. The build phase
/// leaves out what nextest ignores or refuses with `--no-run`, so no warning
/// about it shows: heli's reporter flags, the user's `--no-fail-fast` and
/// `--retries`, and the thread count and retries of the environment.
#[test]
fn a_failing_nextest_test_shows_its_name_panic_and_output() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let output = fixture
        .heli(["nextest", "run", "-p", "tests-mixed", "--no-fail-fast"])
        .args(["--retries", "0"])
        .env("NEXTEST_TEST_THREADS", "2")
        .env("NEXTEST_RETRIES", "0")
        .output()
        .expect("run heli");
    assert_eq!(
        output.status.code(),
        Some(100),
        "nextest's code: {output:?}"
    );
    assert_eq!(text(&output.stdout), "", "nothing on stdout");
    let stderr = support::without_footer(&output.stderr);
    for shown in [
        "FAIL [",
        // nextest counts finished tests, and with two threads the failing
        // one may finish first or second: match the total, not the position.
        "/3) tests-mixed tests::fails\n",
        "captured output of the failing test\n",
        "fixture failure: expected to fail\n",
        " tests run: 2 passed, 1 failed, 0 skipped\n",
    ] {
        assert!(stderr.contains(shown), "{shown:?} in {stderr}");
    }
    for hidden in ["PASS [", "warning"] {
        assert!(!stderr.contains(hidden), "no {hidden:?} in {stderr}");
    }
}

/// `cargo heli <args>` with `echo-args` as cargo, which prints the arguments
/// of each phase and the variable `ECHO_ENV` names; it must succeed. Its
/// stdout, a line each.
fn echoed(fixture: &Fixture, args: &[&str], env: &[(&str, &str)]) -> Vec<String> {
    let output = fixture
        .heli(args)
        .env("CARGO", fixture.echo_args())
        .env_remove("NEXTEST_TEST_THREADS")
        .envs(env.iter().copied())
        .output()
        .expect("run heli");
    support::assert_success(&output, &format!("cargo heli {args:?}"));
    support::lines(&output.stdout)
}

/// `arg: "<arg>"` for each of `args`, as `echo-args` prints them.
fn args_of(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| format!("arg: {arg:?}")).collect()
}

/// heli adds each of nextest's controls only when the user set it neither
/// by flag nor by variable; the build phase gets none of the reporter's,
/// nor their variables.
#[test]
fn nextest_controls_the_user_set_are_honoured() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let printed = echoed(
        &fixture,
        &[
            "nextest",
            "run",
            "--status-level",
            "all",
            "--success-output=immediate",
            "--cargo-message-format",
            "human",
        ],
        &[
            ("NEXTEST_FAILURE_OUTPUT", "immediate-final"),
            ("ECHO_ENV", "NEXTEST_FAILURE_OUTPUT"),
        ],
    );
    let mut expected = args_of(&[
        "nextest",
        "run",
        "--no-run",
        "--cargo-message-format",
        "human",
        "--cargo-quiet",
    ]);
    expected.push("env NEXTEST_FAILURE_OUTPUT=None".into());
    expected.extend(args_of(&[
        "nextest",
        "run",
        "--test-threads",
        "2",
        "--status-level",
        "all",
        "--success-output=immediate",
        "--cargo-message-format",
        "human",
        "--cargo-quiet",
        "--final-status-level",
        "fail",
    ]));
    expected.push("env NEXTEST_FAILURE_OUTPUT=Some(\"immediate-final\")".into());
    assert_eq!(printed, expected);

    // A run that only builds gets cargo's controls only; one that does not
    // capture (`--nocapture` after `--` too) gets no output levels; the
    // user's `--cargo-verbose` keeps cargo's lines.
    let printed = echoed(&fixture, &["nextest", "run", "--no-run"], &[]);
    let expected = args_of(&[
        "nextest",
        "run",
        "--no-run",
        "--cargo-quiet",
        "--cargo-message-format",
        "json",
    ]);
    assert_eq!(printed, expected);
    let printed = echoed(
        &fixture,
        &["nextest", "run", "--cargo-verbose", "--", "--nocapture"],
        &[],
    );
    let expected = args_of(&[
        "nextest",
        "run",
        "--no-run",
        "--cargo-verbose",
        "--cargo-message-format",
        "json",
        "--",
        "nextest",
        "run",
        "--test-threads",
        "2",
        "--cargo-verbose",
        "--cargo-message-format",
        "json",
        "--status-level",
        "fail",
        "--final-status-level",
        "fail",
        "--",
        "--nocapture",
    ]);
    assert_eq!(printed, expected);
}

/// nextest's build phase leaves out every flag and variable that only the
/// tests read, which nextest ignores (with a warning) or refuses under
/// `--no-run`; the test phase keeps them, the thread count aside, which it
/// sets. In full mode, so that heli adds nothing.
#[test]
fn nextests_build_leaves_out_what_only_the_tests_read() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let run_only = [
        "-j",
        "2",
        "--test-threads=2",
        "--jobs",
        "2",
        "--retries",
        "1",
        "--flaky-result",
        "pass",
        "--no-tests=pass",
        "--max-fail",
        "1",
        "--status-level",
        "all",
        "--final-status-level",
        "all",
        "--success-output",
        "never",
        "--failure-output",
        "final",
        "--no-fail-fast",
        "--fail-fast",
        "--no-capture",
        "--nocapture",
    ];
    let args: Vec<&str> = ["--output", "full", "nextest", "run"]
        .iter()
        .chain(&run_only)
        .chain(&["-p", "x", "--", "f", "--nocapture", "--no-capture"])
        .copied()
        .collect();
    let mut expected = args_of(&["nextest", "run", "--no-run", "-p", "x", "--", "f"]);
    let kept: Vec<&str> = ["nextest", "run", "--test-threads", "2"]
        .iter()
        .chain(&run_only[5..])
        .chain(&["-p", "x", "--", "f", "--nocapture", "--no-capture"])
        .copied()
        .collect();
    expected.extend(args_of(&kept));
    assert_eq!(echoed(&fixture, &args, &[]), expected);

    for var in [
        "NEXTEST_TEST_THREADS",
        "NEXTEST_RETRIES",
        "NEXTEST_FLAKY_RESULT",
        "NEXTEST_NO_TESTS",
        "NEXTEST_STATUS_LEVEL",
        "NEXTEST_FINAL_STATUS_LEVEL",
        "NEXTEST_SUCCESS_OUTPUT",
        "NEXTEST_FAILURE_OUTPUT",
    ] {
        let printed = echoed(
            &fixture,
            &["--output", "full", "nextest", "run"],
            &[(var, "1"), ("ECHO_ENV", var)],
        );
        let env: Vec<&String> = printed
            .iter()
            .filter(|line| line.starts_with("env "))
            .collect();
        assert_eq!(
            env,
            [
                &format!("env {var}=None"),
                &format!("env {var}=Some(\"1\")")
            ],
            "{var}"
        );
    }
}

/// Output levels the user sets show what they ask for in compact mode:
/// here every passing test's line and its output.
#[test]
fn nextest_output_levels_the_user_set_are_shown() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let output = heli(
        &fixture,
        &[
            "nextest",
            "run",
            "-p",
            "many-tests",
            "-E",
            "test(=g0::t0)",
            "--status-level",
            "pass",
            "--success-output",
            "immediate",
        ],
    );
    support::assert_success(&output, "cargo heli nextest run");
    let (stderr, footer) = support::split_footer(&output.stderr);
    let stderr = text(&stderr);
    for shown in [
        "PASS [",
        "] (1/1) many-tests g0::t0\n",
        "output of many_tests::g0::t0\n",
    ] {
        assert!(stderr.contains(shown), "{shown:?} in {stderr}");
    }

    // `log --test` shows the test's whole output, lines that look like
    // nextest's status lines included, to libtest's summary.
    let logged = log(&fixture, &[&footer.run, "--test", "g0::t0"]);
    support::assert_success(&logged, "cargo heli log --test");
    let logged = text(&logged.stderr);
    assert!(
        logged.trim_start().starts_with("PASS [")
            && logged.contains(&printed_by("g0::t0", "    "))
            && logged
                .lines()
                .last()
                .is_some_and(|last| last.starts_with("    test result: ok. 1 passed;")),
        "{logged}"
    );
    assert!(stderr.contains(&logged), "what the run printed: {logged}");
}

/// `text` without what differs between two runs of the same tests: the id
/// of a panicking thread (`thread 'x' (1234) panicked`) and how long
/// libtest took (`finished in 0.30s`).
fn untimed(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let mut line = line.to_owned();
            if let Some(start) = line.find("' (")
                && let Some(length) = line[start + 3..].find(") panicked")
            {
                line.replace_range(start + 3..start + 3 + length, "N");
            }
            if let Some(at) = line.find("; finished in ") {
                line.truncate(at);
                line.push('\n');
            }
            line
        })
        .collect()
}

/// The lines of libtest's output `stdout` that compact mode keeps: from
/// each `failures:` or `successes:` line to the `test result:` line after
/// it, and the `test result:` lines of binaries whose tests failed.
fn kept_sections(stdout: &str) -> String {
    let mut kept = String::new();
    let mut keeping = false;
    for line in stdout.split_inclusive('\n') {
        keeping |= line == "failures:\n" || line == "successes:\n";
        if line.starts_with("test result: ") {
            keeping = false;
            if !line.starts_with("test result: ok.") {
                kept.push_str(line);
            }
        }
        if keeping {
            kept.push_str(line);
        }
    }
    kept
}

/// In compact mode, `cargo test` keeps libtest's `failures:` sections, with
/// each failing test's captured output and panic message, and the summary
/// of the test binary that failed, as plain cargo prints them; it drops the
/// lines per test. heli's last line counts the tests.
#[test]
fn libtest_failures_are_shown_in_compact_mode() {
    let fixture = Fixture::new("basic");
    let plain = fixture
        .cargo(["test", "-p", "tests-mixed"])
        .env_remove("RUST_TEST_THREADS")
        .output()
        .expect("run cargo");
    assert_eq!(plain.status.code(), Some(101), "{plain:?}");
    let expected = kept_sections(&text(&plain.stdout));
    assert!(
        expected.contains("captured output of the failing test\n")
            && expected.contains("fixture failure: expected to fail\n")
            && expected.contains("test result: FAILED. 2 passed; 1 failed;"),
        "plain cargo's failures: {expected}"
    );

    let output = heli(&fixture, &["test", "-p", "tests-mixed"]);
    assert_eq!(output.status.code(), Some(101), "cargo's code: {output:?}");
    assert_eq!(untimed(&text(&output.stdout)), untimed(&expected));
    let stderr = support::without_footer(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 3, "{stderr}");
    assert!(lines[0].contains(": testing on "), "the header: {stderr}");
    assert_eq!(
        lines[1..],
        [
            "error: test failed, to rerun pass `-p tests-mixed --lib`",
            "tests: 2 passed, 1 failed"
        ]
    );
}

/// A green `cargo test` prints only heli's lines in compact mode: the test
/// header and the count of tests, summed over the test binaries (here the
/// library's and its doc tests'). In quiet mode, one line with the count.
#[test]
fn a_green_libtest_run_prints_its_count() {
    let fixture = Fixture::new("basic");
    let output = heli(&fixture, &["test", "-p", "many-tests"]);
    support::assert_success(&output, "cargo heli test");
    assert_eq!(text(&output.stdout), "", "nothing on stdout");
    let stderr = support::without_footer(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    assert!(lines[0].contains(": testing on "), "the header: {stderr}");
    assert_eq!(lines[1], "tests: 100 passed, 1 ignored");

    let output = heli(&fixture, &["-q", "--no-cache", "test", "-p", "many-tests"]);
    quiet_line(&output, "test -p many-tests", "100 passed, 1 ignored, ");
}

/// What test `test` of `many-tests` prints, each line after `indent`.
fn printed_by(test: &str, indent: &str) -> String {
    [
        &format!("output of many_tests::{test}"),
        "NOT [a] status line",
        "not a: status [   1.000s] line",
        "... not progress",
        "running two tests",
    ]
    .iter()
    .map(|line| format!("{indent}{line}\n"))
    .collect()
}

/// `cargo heli test <args>` of `many-tests`, which must pass: its stdout,
/// and its stderr without the footer, whose last line counts the tests.
fn green_libtest(fixture: &Fixture, args: &[&str], counts: &str) -> (String, String) {
    let args: Vec<&str> = ["test", "-p", "many-tests"]
        .iter()
        .chain(args)
        .copied()
        .collect();
    let output = heli(fixture, &args);
    support::assert_success(&output, &format!("cargo heli {args:?}"));
    let (stderr, footer) = support::split_footer(&output.stderr);
    let stderr = text(&stderr);
    assert!(
        stderr.ends_with(&format!("\ntests: {counts}\n")),
        "{stderr}"
    );
    (text(&output.stdout), footer.run)
}

/// Under `--show-output`, libtest lists what passing tests printed in a
/// `successes:` section: compact mode keeps it, as plain cargo prints it,
/// and `log --test` shows one test's part of it, to the next test's or to
/// the list of names. The count names the tests that libtest's filter left
/// out, the ignored one among them. On one thread, so that the sections
/// come in the same order in both runs.
#[test]
fn libtest_successes_are_kept_and_filtered_tests_counted() {
    let fixture = Fixture::new("basic");
    let args = ["--", "g0::t", "--show-output", "--test-threads=1"];
    let plain = fixture
        .cargo(["test", "-p", "many-tests"].iter().chain(&args))
        .env_remove("RUST_TEST_THREADS")
        .output()
        .expect("run cargo");
    support::assert_success(&plain, "cargo test");
    let expected = kept_sections(&text(&plain.stdout));
    let section = |test: &str| format!("---- {test} stdout ----\n{}", printed_by(test, ""));
    assert!(
        expected.contains(&section("g0::t0")) && expected.contains(&section("g0::t9")),
        "plain cargo's: {expected}"
    );

    let (stdout, run) = green_libtest(&fixture, &args, "10 passed, 91 filtered out");
    assert_eq!(stdout, expected);
    for test in ["g0::t0", "g0::t9"] {
        let logged = log(&fixture, &[&run, "--test", test]);
        support::assert_success(&logged, "cargo heli log --test");
        assert_eq!(text(&logged.stdout), section(test));
    }
}

/// libtest's lines per test are dropped in its pretty format too; what a
/// test prints itself (`--nocapture`) passes through, though it looks like
/// libtest's own lines. When the user's arguments choose cargo's output
/// (`--quiet`), all of libtest's output passes through.
#[test]
fn libtest_drops_only_its_lines_per_test() {
    let fixture = Fixture::new("basic");
    let (stdout, _) = green_libtest(
        &fixture,
        &["--", "g0::", "--format", "pretty"],
        "10 passed, 91 filtered out",
    );
    assert_eq!(stdout, "");
    let (stdout, _) = green_libtest(&fixture, &["--", "no::such"], "0 passed, 101 filtered out");
    assert_eq!(stdout, "");
    let (stdout, _) = green_libtest(
        &fixture,
        &["--", "g0::t0", "--nocapture"],
        "1 passed, 100 filtered out",
    );
    assert_eq!(stdout, printed_by("g0::t0", ""));

    let output = heli(&fixture, &["test", "-p", "many-tests", "--quiet"]);
    support::assert_success(&output, "cargo heli test --quiet");
    assert!(
        text(&output.stdout).contains("\nrunning 101 tests\n"),
        "libtest's own lines: {}",
        text(&output.stdout)
    );
    let stderr = support::without_footer(&output.stderr);
    assert!(!stderr.contains("tests: "), "no count of heli's: {stderr}");
}

/// The build phase of a test run shows cargo's diagnostics as a compile run
/// does, whichever the runner: errors as rustc renders them, on stderr,
/// and nothing of cargo's JSON; no tests ran, so heli counts none.
#[test]
fn a_test_runs_build_errors_show_as_a_compile_runs() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let plain = fixture
        .cargo(["build", "-p", "broken"])
        .output()
        .expect("run cargo");
    let plain = text(&plain.stderr);
    let error = plain
        .split_inclusive('\n')
        .skip_while(|line| !line.starts_with("error[E0308]"))
        .take_while(|line| !line.trim().is_empty())
        .collect::<String>();
    assert!(error.contains("expected `u32`"), "plain cargo's: {plain}");
    for runner in [&["test"][..], &["nextest", "run"]] {
        let args: Vec<&str> = runner.iter().chain(&["-p", "broken"]).copied().collect();
        let output = heli(&fixture, &args);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert_eq!(text(&output.stdout), "", "{args:?}");
        let stderr = support::without_footer(&output.stderr);
        assert!(stderr.contains(&error), "{args:?}: {stderr}");
        assert!(!stderr.contains("tests: "), "{args:?}: {stderr}");
    }
}

/// `cargo heli log` with `args`.
fn log(fixture: &Fixture, args: &[&str]) -> Output {
    fixture
        .heli(["log"])
        .args(args)
        .output()
        .expect("run heli log")
}

/// `cargo heli <args>`, a run of `tests-mixed` whose test `tests::fails`
/// fails, without a backtrace: its output and the run its footer names.
fn failing_run(fixture: &Fixture, args: &[&str]) -> (Output, String) {
    let output = fixture
        .heli(args)
        .env_remove("NEXTEST_TEST_THREADS")
        .env_remove("RUST_TEST_THREADS")
        .env_remove("RUST_BACKTRACE")
        .output()
        .expect("run heli");
    assert!(!output.status.success(), "a test fails: {output:?}");
    let (_, footer) = support::split_footer(&output.stderr);
    (output, footer.run)
}

/// What the failing test of `tests-mixed` prints on stdout, after `indent`.
fn failing_test_stdout(indent: &str) -> String {
    format!("{indent}captured output of the failing test\n")
}

/// The panic of the failing test of `tests-mixed`, each line after
/// `indent`, the thread's id as `N`.
fn failing_test_panic(indent: &str) -> String {
    let sep = std::path::MAIN_SEPARATOR_STR;
    [
        &format!(
            "thread 'tests::fails' (N) panicked at crates{sep}tests-mixed{sep}src{sep}lib.rs:20:9:"
        ),
        "assertion `left == right` failed: fixture failure: expected to fail",
        "  left: 4",
        " right: 5",
        "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
    ]
    .iter()
    .map(|line| format!("{indent}{line}\n"))
    .collect()
}

/// What the failing test of `tests-mixed` printed, as libtest shows it when
/// it captures the output (libtest's own `failures:` section): the line it
/// printed, a blank line, then the panic.
fn failing_test_output() -> String {
    format!("{}\n{}", failing_test_stdout(""), failing_test_panic(""))
}

/// `log <run> --test <name>` prints one test's output from a libtest run's
/// record: its section, `---- <name> stdout ----` and what libtest captured,
/// on stdout where libtest wrote it.
#[test]
fn log_test_shows_one_libtest_tests_output() {
    let fixture = Fixture::new("basic");
    let (_, run) = failing_run(&fixture, &["test", "-p", "tests-mixed"]);
    let logged = log(&fixture, &[&run, "--test", "tests::fails"]);
    support::assert_success(&logged, "cargo heli log --test");
    assert_eq!(
        untimed(&text(&logged.stdout)),
        format!("---- tests::fails stdout ----\n{}", failing_test_output())
    );
    assert_eq!(text(&logged.stderr), "");
}

/// `log <run> --test <name>` prints one test's output from a nextest run's
/// record, on stderr where nextest wrote it: the test's status line and the
/// output under it. By the test's name, or by its binary and name. In a
/// compact run, the output comes once, after the summary; in a full run
/// (here without nextest's counter of tests, `(1/3)`), right after the
/// test's line, and a passing test has its line only.
#[test]
fn log_test_shows_one_nextest_tests_output() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    for (mode, name, progress) in [
        ("compact", "tests::fails", "counter"),
        ("full", "tests-mixed tests::fails", "none"),
    ] {
        let (output, run) = failing_run(
            &fixture,
            &[
                "--output",
                mode,
                "nextest",
                "run",
                "-p",
                "tests-mixed",
                "--no-fail-fast",
                "--show-progress",
                progress,
            ],
        );
        let logged = log(&fixture, &[&run, "--test", name]);
        support::assert_success(&logged, "cargo heli log --test");
        assert_eq!(text(&logged.stdout), "", "{mode}");
        let stderr = text(&logged.stderr);
        let (status, rest) = stderr.split_once('\n').expect("a status line");
        assert!(
            status.trim_start().starts_with("FAIL [")
                && status.ends_with(" tests-mixed tests::fails"),
            "{mode}: {stderr}"
        );
        let rest = untimed(rest);
        assert!(
            rest.contains(&failing_test_stdout("    "))
                && rest.ends_with(&failing_test_panic("    ")),
            "{mode}: {stderr}"
        );
        assert!(
            text(&output.stderr).contains(&stderr),
            "{mode}: what the run printed: {stderr}"
        );
        for other in ["Summary [", "PASS [", "warning:", "error:", "Cancelling"] {
            assert!(!stderr.contains(other), "{mode}: no {other:?}: {stderr}");
        }
    }

    let (_, run) = failing_run(
        &fixture,
        &[
            "--output",
            "full",
            "nextest",
            "run",
            "-p",
            "tests-mixed",
            "--no-fail-fast",
        ],
    );
    let logged = log(&fixture, &[&run, "--test", "tests::passes"]);
    support::assert_success(&logged, "cargo heli log --test");
    let stderr = text(&logged.stderr);
    assert!(
        stderr.trim_start().starts_with("PASS [")
            && stderr.ends_with(" tests-mixed tests::passes\n")
            && stderr.lines().count() == 1,
        "{stderr}"
    );
}

/// A test's output may end the record: with the outputs of passing tests
/// at the end (`--success-output final`), nothing of nextest's follows the
/// last one. `log --test` shows each test's, the last included.
#[test]
fn log_test_shows_an_output_that_ends_the_record() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let output = heli(
        &fixture,
        &[
            "nextest",
            "run",
            "-p",
            "test-slots",
            "--success-output",
            "final",
            "--final-status-level",
            "pass",
        ],
    );
    support::assert_success(&output, "cargo heli nextest run");
    let run = support::split_footer(&output.stderr).1.run;
    for test in ["a", "b", "c", "d"] {
        let name = format!("tests::{test}");
        let logged = log(&fixture, &[&run, "--test", &name]);
        support::assert_success(&logged, "cargo heli log --test");
        let logged = text(&logged.stderr);
        assert!(
            logged.trim_start().starts_with("PASS [")
                && logged
                    .lines()
                    .next()
                    .is_some_and(|line| line.ends_with(&name))
                && logged
                    .lines()
                    .last()
                    .is_some_and(|last| last.starts_with("    test result: ok. 1 passed;")),
            "{name}: {logged}"
        );
    }
}

/// A test the record holds no output of (a passing test in a compact
/// nextest run, or none of that name) gets one line saying so; `--test` is
/// one selector among the others.
#[test]
fn log_test_of_a_test_without_output_says_so() {
    support::require_nextest();
    let fixture = Fixture::new("basic");
    let (_, run) = failing_run(&fixture, &["nextest", "run", "-p", "tests-mixed"]);
    for name in ["tests::passes", "tests::none"] {
        let logged = log(&fixture, &[&run, "--test", name]);
        support::assert_success(&logged, "cargo heli log --test");
        assert_eq!(text(&logged.stdout), "");
        assert_eq!(
            text(&logged.stderr),
            format!("cargo-heli: no output of test `{name}` in {run}\n")
        );
    }
    let both = log(&fixture, &[&run, "--test", "tests::fails", "--errors"]);
    assert_eq!(both.status.code(), Some(2), "two selectors: {both:?}");
}

/// `cargo heli exec -- chatter <args>`: its output, and the run its footer
/// names.
fn chatter(fixture: &Fixture, flags: &[&str], args: &[&str]) -> (Output, String) {
    let output = fixture
        .heli(flags)
        .arg(fixture.binary("chatter"))
        .args(args)
        .output()
        .expect("run heli");
    support::assert_success(&output, &format!("cargo heli exec -- chatter {args:?}"));
    let run = support::split_footer(&output.stderr).1.run;
    (output, run)
}

/// The lines chatter numbers `numbers` as it writes them on `channel`:
/// odd ones on stdout, even ones on stderr.
fn chatter_lines(numbers: impl IntoIterator<Item = u32>, stdout: bool) -> String {
    numbers
        .into_iter()
        .filter(|i| (i % 2 == 1) == stdout)
        .map(|i| {
            if stdout {
                format!("line {i} out\n")
            } else {
                format!("line {i} err\r\n")
            }
        })
        .collect()
}

/// In compact mode, `exec` keeps the first 20 and the last 60 lines of the
/// output, of both streams together, each on its own stream; a line on
/// stderr between them says how many lines were cut and where they all are.
/// The record holds everything.
#[test]
fn compact_exec_output_keeps_the_head_and_the_tail() {
    let fixture = Fixture::new("basic");
    let (output, run) = chatter(&fixture, &["exec", "--"], &["100", "pace=20"]);
    let head = 1..=20;
    let tail = 41..=100;
    assert_eq!(
        text(&output.stdout),
        chatter_lines(head.clone().chain(tail.clone()), true)
    );
    assert_eq!(
        support::without_footer(&output.stderr),
        format!(
            "{}cargo-heli: 20 lines cut (cargo heli log {run} has them all)\n{}",
            chatter_lines(head, false),
            chatter_lines(tail, false)
        )
    );
    let logged = log(&fixture, &[&run]);
    support::assert_success(&logged, "cargo heli log");
    assert_eq!(text(&logged.stdout), chatter_lines(1..=100, true));
    assert_eq!(text(&logged.stderr), chatter_lines(1..=100, false));

    // 80 lines are shown whole; of 81, one is cut.
    for (count, cut) in [("80", None), ("81", Some("1 line cut"))] {
        let (output, _) = chatter(&fixture, &["exec", "--"], &[count]);
        let shown = support::lines(&output.stdout).len()
            + support::without_footer(&output.stderr).lines().count();
        let marker = support::without_footer(&output.stderr)
            .lines()
            .find(|line| line.starts_with("cargo-heli: "))
            .map(str::to_owned);
        match cut {
            None => assert_eq!((shown, marker), (80, None), "{count} lines"),
            Some(cut) => {
                assert_eq!(shown, 81, "80 lines and the marker");
                let marker = marker.expect("a marker");
                assert!(
                    marker.starts_with(&format!("cargo-heli: {cut} ")),
                    "{marker}"
                );
            }
        }
    }
}

/// In quiet mode, an `exec` that succeeds prints one line, with how many
/// lines of output its record holds; `-q` may come before or after `exec`.
/// A failing one prints the compact output, then the footer.
#[test]
fn a_quiet_exec_prints_one_line() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    for flags in [&["-q", "exec", "--"], &["exec", "-q", "--"]] {
        let output = fixture
            .heli(flags)
            .arg(&echo)
            .arg("through")
            .output()
            .expect("run heli");
        let run = quiet_line(&output, "echo-args through", "2 lines, ");
        let logged = log(&fixture, &[&run]);
        assert_eq!(text(&logged.stdout), "arg: \"through\"\n");
    }

    let output = fixture
        .heli(["-q", "exec", "--"])
        .arg(&echo)
        .env("ECHO_EXIT", "3")
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        support::without_footer(&output.stderr),
        "echo-args: stderr\n"
    );
}
