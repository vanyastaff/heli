//! nextest's test runs, compact: the view of `cargo heli nextest run` in
//! compact and quiet mode (docs/runs.md, "Output").
//!
//! **nextest's own controls**, which heli adds to the command
//! ([`with_controls`]) unless the user set them, by flag or by the variable
//! nextest reads for it:
//!
//! - `--cargo-quiet` (not with `--cargo-quiet` or `--cargo-verbose`
//!   already): cargo prints no line per unit;
//! - `--cargo-message-format json`: cargo's diagnostics come as JSON on
//!   stdout, rendered as for a compile run ([`Diagnostics`], warnings
//!   counted per lint);
//! - `--status-level fail` and `--final-status-level fail`
//!   (`NEXTEST_STATUS_LEVEL`, `NEXTEST_FINAL_STATUS_LEVEL`): no line per
//!   passing test;
//! - `--success-output never` and `--failure-output final`
//!   (`NEXTEST_SUCCESS_OUTPUT`, `NEXTEST_FAILURE_OUTPUT`): a failing test's
//!   output (its stdout, and its stderr with the panic message) once, after
//!   the summary, under its name.
//!
//! The reporter's four are left out of a command that only builds
//! (`--no-run`), and the two outputs out of one that does not capture
//! (`--no-capture` or `--nocapture`, also after `--`): nextest ignores them
//! there, with a warning. A test run's build phase leaves them out too
//! (`crate::daemon::test_run`).
//!
//! **Rendering** ([`Nextest`]): nextest's own output, on stderr, passes
//! through; so do lines on stdout that are not cargo's JSON (a test's output
//! under `--no-capture`). Quiet mode's line shows the counts of nextest's
//! summary line (`812 passed, 3 skipped`), then the warnings.

use std::ffi::OsString;

use super::cargo::{Diagnostics, Warnings};
use super::{LineBuffer, Render, Sink};
use crate::protocol::Channel;

/// The reporter's controls heli sets: the flag, its value, the variable
/// nextest also reads it from, and whether nextest ignores it for tests
/// that do not capture their output.
const REPORTER: [(&str, &str, &str, bool); 4] = [
    ("--status-level", "fail", "NEXTEST_STATUS_LEVEL", false),
    (
        "--final-status-level",
        "fail",
        "NEXTEST_FINAL_STATUS_LEVEL",
        false,
    ),
    ("--success-output", "never", "NEXTEST_SUCCESS_OUTPUT", true),
    ("--failure-output", "final", "NEXTEST_FAILURE_OUTPUT", true),
];

/// `cargo <args>` (`args` from `nextest` on, after a `+toolchain`) with
/// nextest's controls that the user did not set (see the module docs):
/// neither in `args` nor in a variable `set` says the environment sets.
/// They go after the user's arguments and before any `--`, as cargo's
/// flags do (`super::cargo::with_json_messages`).
pub fn with_controls(args: &[OsString], set: impl Fn(&str) -> bool) -> Vec<OsString> {
    let at = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let has = |flag: &str| args[..at].iter().any(|arg| is_flag(arg, flag));
    let builds_only = has("--no-run");
    let uncaptured = args
        .iter()
        .any(|arg| arg == "--no-capture" || arg == "--nocapture");
    let mut added: Vec<&str> = Vec::new();
    if !has("--cargo-quiet") && !has("--cargo-verbose") {
        added.push("--cargo-quiet");
    }
    if !has("--cargo-message-format") {
        added.extend(["--cargo-message-format", "json"]);
    }
    for (flag, value, var, captured_only) in REPORTER {
        if !builds_only && !(captured_only && uncaptured) && !has(flag) && !set(var) {
            added.extend([flag, value]);
        }
    }
    let mut with = args[..at].to_vec();
    with.extend(added.into_iter().map(OsString::from));
    with.extend_from_slice(&args[at..]);
    with
}

/// Whether `arg` is `flag`, alone or with its value (`--flag=value`).
fn is_flag(arg: &OsString, flag: &str) -> bool {
    arg.to_str().is_some_and(|arg| {
        arg.strip_prefix(flag)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
    })
}

/// The counts of nextest's summary line, `     Summary [   0.323s] 2 tests
/// run: 2 passed, 1 skipped`: `2 passed, 1 skipped`.
fn summary_counts(line: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(line).ok()?.trim();
    let rest = line.strip_prefix("Summary [")?;
    let (_, rest) = rest.split_once("] ")?;
    let (_, counts) = rest.split_once(" run: ")?;
    Some(counts.to_owned())
}

/// What follows the time on a status line of nextest's (without its line
/// end): `(1/3) tests-mixed tests::fails` on `        FAIL [   0.029s] (1/3)
/// tests-mixed tests::fails`, `2 tests run: …` on `     Summary [
/// 0.323s] 2 tests run: …`. `None` for any other line: a status is words of
/// letters and digits, and a time digits, spaces, `.`, `>` and `s`.
fn status_line(line: &str) -> Option<&str> {
    let (status, rest) = line.trim_start().split_once(" [")?;
    let status_word = |c: char| c.is_ascii_alphanumeric() || c == ' ' || c == '/';
    if status.is_empty() || !status.chars().all(status_word) {
        return None;
    }
    let (time, rest) = rest.split_once("] ")?;
    let time_mark = |c: char| c.is_ascii_digit() || matches!(c, ' ' | '.' | '>' | 's');
    time.chars().all(time_mark).then_some(rest)
}

/// Whether `line` (without its line end) is a status line of test `name`:
/// one that ends with the test's path (`tests::it_works`), or its binary and
/// path (`my-crate tests::it_works`). For `cargo heli log --test`
/// (`crate::runlog::select`).
pub fn is_status_of(line: &str, name: &str) -> bool {
    status_line(line).is_some_and(|test| test == name || test.ends_with(&format!(" {name}")))
}

/// Whether `line` (without its line end) ends the output nextest prints
/// under a test's status line: another status line, or a line of nextest's
/// own (a separator, `warning: …`, `  Cancelling …`), indented less than
/// the output (4 spaces) and not one of the output's headers
/// (`  stdout ───`).
pub fn ends_output(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if status_line(line).is_some() {
        return true;
    }
    let header = line.strip_prefix("  ").is_some_and(|rest| {
        rest.split_once(' ').is_some_and(|(stream, bar)| {
            !stream.is_empty() && stream.chars().all(char::is_alphabetic) && bar.starts_with('─')
        })
    });
    line.len() - trimmed.len() < 4 && !header
}

/// Renders a nextest run (see the module docs).
pub struct Nextest {
    diagnostics: Diagnostics,
    stderr: LineBuffer,
    /// The counts of the last summary line.
    counts: Option<String>,
}

impl Nextest {
    pub fn new() -> Self {
        Self {
            diagnostics: Diagnostics::new(Warnings::Counts),
            stderr: LineBuffer::default(),
            counts: None,
        }
    }

    fn scan(&mut self, line: &[u8]) {
        if let Some(counts) = summary_counts(line) {
            self.counts = Some(counts);
        }
    }
}

impl Render for Nextest {
    fn output(&mut self, channel: Channel, bytes: &[u8], out: &mut Sink) {
        if channel == Channel::Stdout {
            self.diagnostics.output(channel, bytes, out);
            return;
        }
        out.pass(bytes);
        for line in self.stderr.push(bytes) {
            self.scan(&line);
        }
    }

    fn finish(&mut self, run: &str, out: &mut Sink) -> Vec<String> {
        if let Some(line) = self.stderr.rest() {
            self.scan(&line);
        }
        let warnings = self.diagnostics.finish(run, out);
        self.counts.take().into_iter().chain(warnings).collect()
    }
}
