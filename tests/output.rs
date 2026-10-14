//! Output modes (`DESIGN.md` §5, "Output"): what the client prints of a
//! compile or verify run in full, compact and quiet mode, how the mode is
//! chosen, and `cargo heli log <run> --errors`.
//!
//! Expected diagnostics come from plain cargo's own rendering of the same
//! fixture crate, never from heli.

mod support;

use std::process::Output;

use support::{Fixture, terminal};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The diagnostics rustc rendered in plain cargo's human output `stderr`
/// whose header starts with `kind` (`error` or `warning`): each block from
/// its header line to the blank line that ends it, inclusive.
fn rendered_blocks(stderr: &str, kind: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut block: Option<String> = None;
    for line in stderr.split_inclusive('\n') {
        if let Some(open) = &mut block {
            open.push_str(line);
            if line.trim().is_empty() {
                blocks.extend(block.take());
            }
        } else if line.starts_with(&format!("{kind}[")) || line.starts_with(&format!("{kind}: ")) {
            // Cargo's own one-line messages (`error: could not compile …`)
            // have no location and no blank line after them.
            if !line.contains("could not compile") && !line.contains("generated") {
                block = Some(line.to_owned());
            }
        }
    }
    blocks
}

/// The one-line form of an error block: `path:line:col error[code] message`,
/// from its header (`error[code]: message`) and its ` --> path:line:col`.
fn short_error(block: &str) -> String {
    let mut lines = block.lines();
    let header = lines.next().expect("a header");
    let (kind, message) = header.split_once(": ").expect("`kind: message`");
    let location = lines
        .find_map(|line| line.trim_start().strip_prefix("--> "))
        .expect("a location");
    format!("{location} {kind} {message}\n")
}

/// Plain cargo with `args`, its stderr as text; the build must fail.
fn cargo_failing(fixture: &Fixture, args: &[&str]) -> String {
    let output = fixture.cargo(args).output().expect("run cargo");
    assert!(!output.status.success(), "cargo {args:?} must fail");
    text(&output.stderr)
}

/// `stderr` without the line cargo prints when crate `krate` does not
/// compile, which must be there once. It may come before or after rustc's
/// diagnostics, which travel on the other pipe.
fn without_cargo_verdict(stderr: &str, krate: &str) -> String {
    let verdict = format!("error: could not compile `{krate}` (lib) due to ");
    let lines: Vec<&str> = stderr.split_inclusive('\n').collect();
    let verdicts = lines
        .iter()
        .filter(|line| line.starts_with(&verdict))
        .count();
    assert_eq!(verdicts, 1, "cargo's verdict once: {stderr}");
    lines
        .into_iter()
        .filter(|line| !line.starts_with(&verdict))
        .collect()
}

/// heli's stderr without its footer and cargo's verdict on `krate`, and the
/// run the footer names.
fn failed_run(output: &Output, krate: &str) -> (String, String) {
    let (stderr, footer) = support::split_footer(&output.stderr);
    (without_cargo_verdict(&text(&stderr), krate), footer.run)
}

/// `cargo heli log` with `args`.
fn log(fixture: &Fixture, args: &[&str]) -> Output {
    fixture
        .heli(["log"])
        .args(args)
        .output()
        .expect("run heli log")
}

/// Without a terminal, output is compact: of a build with 40 errors, the
/// first 3 as rustc renders them, then 30 as one line each, then how many
/// more there are and how to see them all. Cargo's JSON stays with heli.
#[test]
fn compact_output_shows_three_full_errors_then_thirty_short_ones() {
    let fixture = Fixture::new("basic");
    let plain = cargo_failing(&fixture, &["build", "-p", "many-errors"]);
    let errors = rendered_blocks(&plain, "error");
    assert_eq!(errors.len(), 40, "the fixture's errors: {plain}");

    let output = fixture
        .heli(["build", "-p", "many-errors"])
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(101), "cargo's exit code");
    assert_eq!(text(&output.stdout), "", "nothing on stdout");
    let (stderr, run) = failed_run(&output, "many-errors");
    assert_eq!(stderr, compact_errors(&errors, &run));
}

/// The compact form of run `run`'s 40 `errors`, as plain cargo rendered
/// them.
fn compact_errors(errors: &[String], run: &str) -> String {
    let mut expected = errors[..3].concat();
    for error in &errors[3..33] {
        expected.push_str(&short_error(error));
    }
    expected.push_str(&format!("7 more: cargo heli log {run} --errors\n"));
    expected
}

/// `cargo heli log <run> --errors` prints every error of the run as rustc
/// rendered it, on stderr: of a compact run (whose record holds cargo's
/// JSON), those compact output left out too; of a full run, the errors in
/// cargo's text, each to the blank line after it or to the next warning
/// (cargo's "build failed, waiting for other jobs" right after its verdict,
/// while `slow-a`'s build script sleeps). It is one selector among the
/// others.
#[test]
fn log_errors_shows_every_error() {
    let fixture = Fixture::new("basic");
    let plain = cargo_failing(&fixture, &["build", "-p", "many-errors"]);
    let errors = rendered_blocks(&plain, "error");
    for mode in ["compact", "full"] {
        let mut build = fixture.heli(["--output", mode, "build"]);
        build
            .args(["-p", "many-errors", "-p", "slow-a"])
            .env("CARGO_TARGET_DIR", fixture.path(&format!("target-{mode}")));
        fixture.concurrency_log().attach(&mut build, 3000);
        let output = build.output().expect("run heli");
        let (stderr, run) = failed_run(&output, "many-errors");
        if mode == "full" {
            assert!(stderr.contains("warning: build failed"), "{stderr}");
        }
        let logged = log(&fixture, &[&run, "--errors"]);
        support::assert_success(&logged, "cargo heli log --errors");
        assert_eq!(text(&logged.stdout), "", "{mode}: nothing on stdout");
        let stderr = without_cargo_verdict(&text(&logged.stderr), "many-errors");
        assert_eq!(stderr, errors.concat(), "{mode}: every error");

        let both = log(&fixture, &[&run, "--errors", "--tail", "1"]);
        assert_eq!(both.status.code(), Some(2), "two selectors: {both:?}");
    }
}

/// The warnings of fixture crate `many-warnings`, read from its source: per
/// lint, `path:line:col` of each, in source order. Each line that holds one
/// names it: `let unused_…` (unused_variables, at the name), `{ return …`
/// (clippy::needless_return, at `return`), `fn dead_…` (dead_code, at the
/// name).
fn many_warnings(fixture: &Fixture) -> Vec<(&'static str, Vec<String>)> {
    let path = ["crates", "many-warnings", "src", "lib.rs"];
    let source = std::fs::read_to_string(fixture.workspace().join(path.join("/")))
        .expect("read the fixture");
    let path = path.join(std::path::MAIN_SEPARATOR_STR);
    let mut lints = vec![
        ("unused_variables", "let unused_", 4),
        ("clippy::needless_return", "{ return ", 2),
        ("dead_code", "fn dead_", 3),
    ]
    .into_iter()
    .map(|(lint, marker, skip)| {
        let locations = source
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let column = line.find(marker)? + skip + 1;
                Some(format!("{path}:{}:{column}", index + 1))
            })
            .collect();
        (lint, locations)
    })
    .collect::<Vec<(&str, Vec<String>)>>();
    lints.retain(|(_, locations)| !locations.is_empty());
    lints
}

/// In compact mode, clippy's (and check's) warnings are grouped by lint, the
/// lints with the most first, each with its count and its locations. At
/// most 50 locations are shown, shared out between the lints so that each
/// shows some: of 30, 20 and 10, the first 20, all 20 and all 10.
#[test]
fn compact_clippy_warnings_are_grouped_by_lint() {
    let fixture = Fixture::new("basic");
    let lints = many_warnings(&fixture);
    let counts: Vec<(&str, usize)> = lints
        .iter()
        .map(|(lint, locations)| (*lint, locations.len()))
        .collect();
    assert_eq!(
        counts,
        [
            ("unused_variables", 30),
            ("clippy::needless_return", 20),
            ("dead_code", 10)
        ],
        "the fixture"
    );

    let output = fixture
        .heli(["clippy", "-p", "many-warnings"])
        .output()
        .expect("run heli");
    support::assert_success(&output, "cargo heli clippy");
    assert_eq!(text(&output.stdout), "", "nothing on stdout");
    let (stderr, _) = support::split_footer(&output.stderr);
    let shown = [20, 20, 10];
    let mut expected = "60 warnings:\n".to_owned();
    for ((lint, locations), shown) in lints.iter().zip(shown) {
        let mut line = format!(
            "  {lint} ({}): {}",
            locations.len(),
            locations[..shown].join(", ")
        );
        if shown < locations.len() {
            line.push_str(&format!(", {} more", locations.len() - shown));
        }
        expected.push_str(&line);
        expected.push('\n');
    }
    assert_eq!(text(&stderr), expected);
}

/// The client's stdout and stderr without heli's footer, of a run that
/// succeeded.
fn green_run(fixture: &Fixture, args: &[&str]) -> (String, String) {
    let output = fixture.heli(args).output().expect("run heli");
    support::assert_success(&output, &format!("cargo heli {args:?}"));
    let (stderr, _) = support::split_footer(&output.stderr);
    (text(&output.stdout), text(&stderr))
}

/// In compact mode, a build's warnings are counted per lint, on one line,
/// the lints with the most first, then by name. Check groups them by lint,
/// like clippy, and doc, a compile run like build, prints nothing but the
/// footer when it has no warnings. Cargo's aliases `b`, `c` and `d` count
/// as their subcommands.
#[test]
fn compact_build_warnings_are_counted_per_lint() {
    let fixture = Fixture::new("basic");
    let counted = "4 warnings: 1 dead_code, 1 unused_imports, 1 unused_mut, 1 unused_variables\n";
    for build in ["build", "b"] {
        assert_eq!(
            green_run(&fixture, &[build, "-p", "warnings"]),
            (String::new(), counted.into()),
            "{build}"
        );
    }
    assert_eq!(
        green_run(&fixture, &["build", "-p", "many-warnings"]),
        (
            String::new(),
            "40 warnings: 30 unused_variables, 10 dead_code\n".into()
        )
    );
    assert_eq!(
        green_run(&fixture, &["build", "-p", "chatter"]),
        (String::new(), "1 warning: 1 unused_mut\n".into())
    );
    for check in ["check", "c"] {
        assert_eq!(
            green_run(&fixture, &[check, "-p", "warnings"]),
            (String::new(), warnings_by_lint()),
            "{check}"
        );
    }
    for doc in ["doc", "d"] {
        assert_eq!(
            green_run(&fixture, &[doc, "-p", "warnings"]),
            (String::new(), String::new()),
            "{doc}"
        );
    }
}

/// What follows `--` is the compiler's: heli adds its flags before it, and
/// an argument there with a `v` in it (here `-Aunused_variables`, which
/// allows that lint) does not choose cargo's output.
#[test]
fn arguments_after_the_separator_are_the_compilers() {
    let fixture = Fixture::new("basic");
    let sep = std::path::MAIN_SEPARATOR_STR;
    let at =
        |line: u32, column: u32| format!("crates{sep}warnings{sep}src{sep}lib.rs:{line}:{column}");
    assert_eq!(
        green_run(
            &fixture,
            &["clippy", "-p", "warnings", "--", "-Aunused_variables"]
        ),
        (
            String::new(),
            format!(
                "4 warnings:\n  clippy::let_and_return (1): {}\n  dead_code (1): {}\n  \
                 unused_imports (1): {}\n  unused_mut (1): {}\n",
                at(9, 5),
                at(12, 4),
                at(4, 5),
                at(8, 9)
            )
        )
    );
}

/// A `+toolchain` before the subcommand is rustup's: heli reads the
/// subcommand after it. Here `echo-args` stands in for cargo and prints the
/// arguments heli gave it: its JSON flags for `+pinned check`, none when
/// the user chose cargo's output.
#[test]
fn a_toolchain_before_the_subcommand_is_looked_past() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    for (args, given) in [
        (
            &["+pinned", "check"][..],
            &["+pinned", "check", "--quiet", "--message-format", "json"][..],
        ),
        (&["+pinned", "check", "-q"], &["+pinned", "check", "-q"]),
    ] {
        let output = fixture
            .heli(args.iter().copied())
            .env("CARGO", &echo)
            .output()
            .expect("run heli");
        support::assert_success(&output, &format!("cargo heli {args:?}"));
        let echoed: String = given.iter().map(|arg| format!("arg: {arg:?}\n")).collect();
        assert_eq!(text(&output.stdout), echoed, "{args:?}");
    }
}

/// The compact output of `check` on fixture crate `warnings`: its four
/// warnings by lint, at the places rustc reports them.
fn warnings_by_lint() -> String {
    let sep = std::path::MAIN_SEPARATOR_STR;
    let at =
        |line: u32, column: u32| format!("crates{sep}warnings{sep}src{sep}lib.rs:{line}:{column}");
    format!(
        "4 warnings:\n  dead_code (1): {}\n  unused_imports (1): {}\n  \
         unused_mut (1): {}\n  unused_variables (1): {}\n",
        at(12, 4),
        at(4, 5),
        at(8, 9),
        at(7, 9)
    )
}

/// `stderr` without cargo's own lines on a build of crate `krate` that
/// failed, of which there must be one or more: `could not compile`, once
/// per target, and `build failed, waiting for other jobs`.
fn without_cargo_lines(stderr: &str, krate: &str) -> String {
    let ours = |line: &&str| {
        line.starts_with(&format!("error: could not compile `{krate}` "))
            || line.starts_with("warning: build failed")
    };
    let lines: Vec<&str> = stderr.split_inclusive('\n').collect();
    assert!(lines.iter().any(ours), "cargo's verdict: {stderr}");
    lines.into_iter().filter(|line| !ours(line)).collect()
}

/// A diagnostic that several targets report (here the library and its unit
/// tests, which compile the same source) is shown once: in compact output,
/// warnings and errors, and by `log --errors`.
#[test]
fn a_diagnostic_two_targets_report_is_shown_once() {
    let fixture = Fixture::new("basic");
    assert_eq!(
        green_run(&fixture, &["check", "--all-targets", "-p", "warnings"]),
        (String::new(), warnings_by_lint())
    );

    let plain = cargo_failing(&fixture, &["build", "-p", "many-errors"]);
    let errors = rendered_blocks(&plain, "error");
    let output = fixture
        .heli(["build", "--all-targets", "-p", "many-errors"])
        .output()
        .expect("run heli");
    let (stderr, footer) = support::split_footer(&output.stderr);
    assert_eq!(
        without_cargo_lines(&text(&stderr), "many-errors"),
        compact_errors(&errors, &footer.run)
    );
    let logged = log(&fixture, &[&footer.run, "--errors"]);
    support::assert_success(&logged, "cargo heli log --errors");
    assert_eq!(
        without_cargo_lines(&text(&logged.stderr), "many-errors"),
        errors.concat()
    );
}

/// `bytes` without the lines of cargo's that differ between runs: the time
/// in `Finished`, and waiting for the package cache, which tests running
/// at the same time share.
fn untimed(bytes: &[u8]) -> String {
    text(bytes)
        .split_inclusive('\n')
        .filter(|line| {
            !line.contains("Finished `dev` profile")
                && !line.contains("Blocking waiting for file lock")
        })
        .collect()
}

/// When the user's arguments choose cargo's output (`--message-format`,
/// `-q`, `-v`, after the subcommand, also among other short flags), heli
/// adds none of its flags (cargo would refuse a second one) and passes the
/// output through: it is what plain cargo prints.
#[test]
fn the_users_output_flags_leave_the_output_alone() {
    let fixture = Fixture::new("basic");
    let warm = fixture
        .cargo(["build", "-p", "warnings"])
        .output()
        .expect("run cargo");
    support::assert_success(&warm, "cargo build");
    for flags in [
        &["--message-format", "short"][..],
        &["--message-format=json"],
        &["-q"],
        &["--quiet"],
        &["-v"],
        &["-vv"],
        &["--verbose"],
        &["-rq"],
    ] {
        let args: Vec<&str> = ["build", "-p", "warnings"]
            .iter()
            .chain(flags)
            .copied()
            .collect();
        let plain = fixture.cargo(&args).output().expect("run cargo");
        let heli = fixture.heli(&args).output().expect("run heli");
        support::assert_success(&heli, &format!("cargo heli {args:?}"));
        assert_eq!(untimed(&heli.stdout), untimed(&plain.stdout), "{flags:?}");
        let (stderr, _) = support::split_footer(&heli.stderr);
        assert_eq!(untimed(&stderr), untimed(&plain.stderr), "{flags:?}");
    }
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
    assert!(
        seconds
            .split_once('.')
            .is_some_and(|(_, tenths)| tenths.len() == 1),
        "seconds to one decimal: {stderr:?}"
    );
    let seconds: f64 = seconds.parse().unwrap_or_else(|_| panic!("{stderr:?}"));
    assert!((0.0..120.0).contains(&seconds), "{stderr:?}");
    run.to_owned()
}

/// A quiet run that succeeds prints one line: the verdict, the command as
/// given, the warnings if any, the duration and the run, whose record holds
/// everything. `-q`, `--output quiet` and `HELI_OUTPUT=quiet` choose it.
#[test]
fn a_quiet_run_that_succeeds_prints_one_line() {
    let fixture = Fixture::new("basic");
    let output = fixture
        .heli(["-q", "build", "-p", "slow-a"])
        .output()
        .expect("run heli");
    let run = quiet_line(&output, "build -p slow-a", "");
    support::assert_success(&log(&fixture, &[&run]), "cargo heli log");

    let output = fixture
        .heli(["--output", "quiet", "check", "-p", "warnings"])
        .output()
        .expect("run heli");
    let run = quiet_line(&output, "check -p warnings", "4 warnings, ");
    let logged = log(&fixture, &[&run, "--grep", "unused_imports"]);
    assert!(!logged.stdout.is_empty(), "the record holds the warnings");

    let output = fixture
        .heli(["build", "-p", "warnings"])
        .env("HELI_OUTPUT", "quiet")
        .output()
        .expect("run heli");
    quiet_line(&output, "build -p warnings", "4 warnings, ");
}

/// A quiet run that fails prints what compact mode prints, and the footer.
#[test]
fn a_quiet_run_that_fails_prints_compact_output() {
    let fixture = Fixture::new("basic");
    let plain = cargo_failing(&fixture, &["build", "-p", "many-errors"]);
    let errors = rendered_blocks(&plain, "error");
    let output = fixture
        .heli(["-q", "build", "-p", "many-errors"])
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(101), "cargo's exit code");
    assert_eq!(text(&output.stdout), "", "nothing on stdout");
    let (stderr, run) = failed_run(&output, "many-errors");
    assert_eq!(stderr, compact_errors(&errors, &run));
}

/// Compact output of a green clippy run over the fixture workspace (the
/// members that compile) is at least 80 % smaller than full output, in
/// bytes. Full output is measured second, on a warm build, when it is
/// smallest: no line per unit compiled.
#[test]
fn compact_output_of_a_green_clippy_run_is_at_least_80_percent_smaller() {
    let fixture = Fixture::new("basic");
    let size = |mode: &str| {
        let output = fixture
            .heli(["--output", mode, "clippy", "--workspace", "--all-targets"])
            .args(["--exclude", "broken", "--exclude", "many-errors"])
            .output()
            .expect("run heli");
        support::assert_success(&output, &format!("cargo heli --output {mode} clippy"));
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

/// Which mode printed `cargo heli build -p warnings`: quiet's one line,
/// compact's counts, or cargo's full rendering.
fn mode_of(output: &Output) -> &'static str {
    support::assert_success(output, "cargo heli build -p warnings");
    let stderr = text(&output.stderr);
    if stderr.starts_with("ok  build -p warnings: 4 warnings, ") {
        return "quiet";
    }
    let body = support::without_footer(&output.stderr);
    if body == "4 warnings: 1 dead_code, 1 unused_imports, 1 unused_mut, 1 unused_variables\n" {
        "compact"
    } else if body.contains("warning: unused import: `std::collections::HashMap`") {
        "full"
    } else {
        panic!("no mode printed this: {stderr}")
    }
}

/// The mode comes from `--output` or `-q` before the subcommand, else from
/// `HELI_OUTPUT`, else from `[output] mode` in the configuration, else from
/// whether stdout is a terminal (here it is not). A mode that is not one,
/// and two flags at once, are usage errors.
#[test]
fn the_mode_comes_from_the_flags_then_the_environment_then_the_configuration() {
    let fixture = Fixture::new("basic");
    let build = |flags: &[&str], env: Option<&str>| {
        let mut command = fixture.heli(flags.iter().chain(&["build", "-p", "warnings"]));
        if let Some(mode) = env {
            command.env("HELI_OUTPUT", mode);
        }
        command.output().expect("run heli")
    };
    assert_eq!(mode_of(&build(&[], None)), "compact", "without a terminal");
    fixture.configure("[output]\nmode = \"full\"\n");
    assert_eq!(mode_of(&build(&[], None)), "full", "configured");
    assert_eq!(mode_of(&build(&[], Some("quiet"))), "quiet", "HELI_OUTPUT");
    assert_eq!(
        mode_of(&build(&["--output", "compact"], Some("quiet"))),
        "compact"
    );
    assert_eq!(mode_of(&build(&["--output=full"], Some("quiet"))), "full");
    assert_eq!(mode_of(&build(&["-q"], Some("full"))), "quiet");

    for (flags, env, named) in [
        (&["--output", "loud"][..], None, "loud"),
        (&[], Some("loud"), "HELI_OUTPUT=loud"),
        (&["-q", "--output", "full"], None, "--output"),
    ] {
        let output = build(flags, env);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{flags:?} {env:?}: {output:?}"
        );
        assert!(output.stdout.is_empty(), "{flags:?} {env:?}: nothing ran");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(named), "{flags:?} {env:?}: {stderr}");
    }
}

/// Output heli does not render passes through in every mode, quiet too,
/// with the footer: here, a cargo subcommand heli knows nothing of, run by
/// `echo-args` standing in for cargo. (`exec`'s output has a view of its
/// own: `tests/test_output.rs`.)
#[test]
fn output_heli_does_not_render_passes_through_in_every_mode() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    for flags in [&["-q"][..], &["--output", "compact"], &["--output", "full"]] {
        let output = fixture
            .heli(flags)
            .args(["tree", "through"])
            .env("CARGO", &echo)
            .output()
            .expect("run heli");
        support::assert_success(&output, "cargo heli tree");
        assert_eq!(
            text(&output.stdout),
            "arg: \"tree\"\narg: \"through\"\n",
            "{flags:?}"
        );
        assert_eq!(
            support::without_footer(&output.stderr),
            "echo-args: stderr\n",
            "{flags:?}"
        );
    }

    // `exec` runs the command as given, even when it is cargo.
    let output = fixture
        .heli(["exec", "--"])
        .arg(support::cargo_binary())
        .args(["build", "-p", "warnings"])
        .output()
        .expect("run heli");
    support::assert_success(&output, "cargo heli exec -- cargo build");
    let stderr = support::without_footer(&output.stderr);
    assert!(
        stderr.contains("warning: unused import: `std::collections::HashMap`"),
        "rustc's full rendering: {stderr}"
    );
}

/// Runs whose output is more than cargo's diagnostics pass through: `run`
/// (a compile run) prints the program's output, and cargo says what it
/// runs; `fmt --check` (a verify run) prints rustfmt's diff; `clippy --fix`
/// (a mutate run) says what it fixed. Cargo says those in lines `--quiet`
/// would drop.
#[test]
fn runs_that_print_more_than_diagnostics_pass_through() {
    let fixture = Fixture::new("basic");
    let output = fixture
        .heli(["run", "-p", "echo-args", "--", "x"])
        .output()
        .expect("run heli");
    support::assert_success(&output, "cargo heli run");
    assert_eq!(text(&output.stdout), "arg: \"x\"\n");
    let stderr = support::without_footer(&output.stderr);
    assert!(stderr.contains("Running `"), "cargo's own lines: {stderr}");

    let output = fixture
        .heli(["fmt", "--check", "-p", "many-warnings"])
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(1), "unformatted: {output:?}");
    let stdout = text(&output.stdout);
    assert!(stdout.contains("Diff in "), "rustfmt's diff: {stdout}");

    let output = fixture
        .heli(["clippy", "--fix", "--allow-no-vcs", "-p", "warnings"])
        .output()
        .expect("run heli");
    support::assert_success(&output, "cargo heli clippy --fix");
    let stderr = support::without_footer(&output.stderr);
    assert!(stderr.contains("Fixed "), "cargo's own lines: {stderr}");
}

/// On a terminal (here a pseudo-terminal), the default is full output. The
/// run itself writes to pipes, so heli sets `CARGO_TERM_COLOR=always` for
/// it, and cargo prints its colours; unless the user's environment sets
/// `CARGO_TERM_COLOR` or `NO_COLOR`. Without a terminal it sets nothing.
#[test]
fn on_a_terminal_the_default_is_full_output_in_colour() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    // The daemon starts outside the terminal, as it does for agents.
    let started = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    support::assert_success(&started, "cargo heli exec");

    let mut build = fixture.heli(["build", "-p", "warnings"]);
    build.env_remove("CARGO_TERM_COLOR").env_remove("NO_COLOR");
    let shown = terminal::run(&mut build);
    assert_eq!(shown.code, Some(0), "{}", shown.text);
    assert!(
        shown.text.contains("std::collections::HashMap"),
        "rustc's full rendering: {}",
        shown.text
    );

    let colour = |set: &[(&str, &str)], terminal: bool| {
        let mut exec = fixture.heli(["exec", "--"]);
        exec.arg(&echo)
            .env("ECHO_ENV", "CARGO_TERM_COLOR")
            .env_remove("CARGO_TERM_COLOR")
            .env_remove("NO_COLOR")
            .envs(set.iter().copied());
        let text = if terminal {
            let shown = terminal::run(&mut exec);
            assert_eq!(shown.code, Some(0), "{}", shown.text);
            shown.text
        } else {
            let output = exec.output().expect("run heli");
            support::assert_success(&output, "cargo heli exec");
            text(&output.stdout)
        };
        let (_, value) = text
            .split_once("CARGO_TERM_COLOR=")
            .unwrap_or_else(|| panic!("echo-args printed no variable: {text}"));
        value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(colour(&[], true), r#"Some("always")"#);
    assert_eq!(
        colour(&[("CARGO_TERM_COLOR", "never")], true),
        r#"Some("never")"#
    );
    assert_eq!(colour(&[("NO_COLOR", "1")], true), "None");
    assert_eq!(colour(&[], false), "None", "without a terminal");
}
