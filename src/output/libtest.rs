//! libtest's test runs, compact: the view of `cargo heli test` in compact
//! and quiet mode (`DESIGN.md` §5, "Output").
//!
//! heli adds cargo's `--quiet --message-format json`, as for a compile run
//! (`super::cargo::with_json_messages`), unless the user's arguments choose
//! cargo's output. Under `--quiet` cargo also has libtest print one
//! character per test. Both phases of a test run, the build and the tests,
//! write to the same view ([`Libtest`]):
//!
//! - cargo's JSON on stdout: rendered as for a compile run
//!   ([`Diagnostics`], warnings counted per lint); a diagnostic both phases
//!   report shows once;
//! - libtest's text on stdout, the only source without structure:
//!   - kept: each `failures:` (or, under `--show-output`, `successes:`)
//!     section up to the test binary's summary line (`test result:`), with
//!     each test's captured output and panic message, and the summary of a
//!     binary whose tests failed;
//!   - dropped: what libtest prints per test (`running 3 tests`, `test x
//!     ... ok`, progress dots, `x --- FAILED`), blank lines between, and the
//!     summary of a binary whose tests passed;
//!   - every other line passes through;
//! - stderr (cargo's own lines, heli's test header) passes through.
//!
//! Once the run is over, a line on stderr sums the summaries of every test
//! binary: `tests: 812 passed, 1 failed, 3 ignored` (passed always, the
//! others when there are some). Quiet mode's line shows the same counts,
//! then the warnings.

use super::cargo::{Diagnostics, Warnings};
use super::{LineBuffer, Render, Sink};
use crate::protocol::Channel;

/// What libtest's summary lines count, summed: each count by what it
/// counts (`passed`, `filtered out`), in the order libtest lists them.
#[derive(Debug, Default)]
struct Counts(Vec<(String, u64)>);

impl Counts {
    /// Adds the counts of a summary line, `test result: ok. 2 passed; 0
    /// failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.30s`;
    /// `None` when `line` is not one. Returns whether it reports failures.
    fn add(&mut self, line: &str) -> Option<bool> {
        let rest = line.strip_prefix("test result: ")?;
        let (verdict, counts) = rest.split_once(". ")?;
        for count in counts.split("; ") {
            let Some((number, what)) = count.split_once(' ') else {
                continue;
            };
            let Ok(number) = number.parse::<u64>() else {
                continue;
            };
            match self.0.iter_mut().find(|(counted, _)| counted == what) {
                Some((_, sum)) => *sum += number,
                None => self.0.push((what.to_owned(), number)),
            }
        }
        Some(verdict != "ok")
    }

    /// `812 passed, 1 failed, 3 ignored`: passed always, the others when
    /// there are some; `None` before any summary line.
    fn describe(&self) -> Option<String> {
        let counts: Vec<String> = self
            .0
            .iter()
            .filter(|(what, sum)| what == "passed" || *sum > 0)
            .map(|(what, sum)| format!("{sum} {what}"))
            .collect();
        (!self.0.is_empty()).then(|| counts.join(", "))
    }
}

/// Whether `line` (without its line end) is what libtest prints per test or
/// between tests, which compact output drops.
fn per_test(line: &str) -> bool {
    let running = line
        .strip_prefix("running ")
        .and_then(|rest| {
            rest.strip_suffix(" tests")
                .or_else(|| rest.strip_suffix(" test"))
        })
        .is_some_and(|count| count.bytes().all(|byte| byte.is_ascii_digit()));
    // `test x ... ok`, `test x ... ignored, reason`, `test x ... FAILED`.
    let pretty = line.starts_with("test ") && line.contains(" ... ");
    // `....... 87/100`: a character per test, then a count at a line's end.
    let (marks, count) = line.split_once(' ').unwrap_or((line, ""));
    let counted = count.is_empty()
        || count
            .split_once('/')
            .is_some_and(|(done, all)| [done, all].iter().all(|n| n.parse::<u64>().is_ok()));
    let terse = !marks.is_empty()
        && marks.bytes().all(|byte| matches!(byte, b'.' | b'i' | b'F'))
        && counted;
    line.is_empty() || running || pretty || terse || line.ends_with(" --- FAILED")
}

/// The test whose section `line` (without its line end) starts, in a
/// `failures:` or `successes:` list: `---- tests::it_works stdout ----`.
/// For `cargo heli log --test` (`crate::runlog::select`).
pub fn section_of(line: &str) -> Option<&str> {
    line.strip_prefix("---- ")?.strip_suffix(" stdout ----")
}

/// Whether `line` (without its line end) ends a test's section: the next
/// section, or the list of names that ends the sections (`failures:`,
/// `successes:`), which libtest prints before the binary's summary.
pub fn ends_section(line: &str) -> bool {
    (line.starts_with("---- ") && line.ends_with(" ----"))
        || line == "failures:"
        || line == "successes:"
}

/// Renders a `cargo test` run (see the module docs).
pub struct Libtest {
    diagnostics: Diagnostics,
    stdout: LineBuffer,
    /// Inside a `failures:` or `successes:` section.
    in_section: bool,
    counts: Counts,
}

impl Libtest {
    pub fn new() -> Self {
        Self {
            diagnostics: Diagnostics::new(Warnings::Counts),
            stdout: LineBuffer::default(),
            in_section: false,
            counts: Counts::default(),
        }
    }

    /// One line of the run's stdout.
    fn line(&mut self, line: &[u8], out: &mut Sink) {
        if !self.in_section && self.diagnostics.json_line(line, out) {
            return;
        }
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches(['\n', '\r']);
        if let Some(failed) = self.counts.add(text) {
            self.in_section = false;
            if failed {
                out.write(Channel::Stdout, line);
            }
            return;
        }
        if text == "failures:" || text == "successes:" {
            self.in_section = true;
        }
        if self.in_section || !per_test(text) {
            out.write(Channel::Stdout, line);
        }
    }
}

impl Render for Libtest {
    fn output(&mut self, channel: Channel, bytes: &[u8], out: &mut Sink) {
        if channel == Channel::Stderr {
            out.pass(bytes);
            return;
        }
        for line in self.stdout.push(bytes) {
            self.line(&line, out);
        }
    }

    fn finish(&mut self, run: &str, out: &mut Sink) -> Vec<String> {
        if let Some(line) = self.stdout.rest() {
            self.line(&line, out);
        }
        let warnings = self.diagnostics.finish(run, out);
        let counts = self.counts.describe();
        if let Some(counts) = &counts {
            out.line(&format!("tests: {counts}"));
        }
        counts.into_iter().chain(warnings).collect()
    }
}
