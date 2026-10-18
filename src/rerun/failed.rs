//! The failed tests of a recorded test run, read from its record
//! (`crate::runlog`) as its runner printed them, in any output mode.
//!
//! - **nextest** ([`nextest`]): its status lines on stderr, `FAIL [ 0.012s]
//!   (1/3) <binary-id> <test>`, right-aligned to 12 columns, with or
//!   without the counter. A test fails with `FAIL`, `TIMEOUT`, `ABORT`,
//!   `EXECFAIL`, `LEAK-FAIL` or a signal (`SIGSEGV`), also as the last word
//!   of a retry's status (`TRY 2 FAIL`); a later `PASS` or `FLAKY` line
//!   for it (a retry that passed) takes it back. nextest prints a failure
//!   again after its summary: each test counts once.
//! - **libtest** ([`libtest`]): on stdout, each test binary's output, from
//!   `running N tests`, with its list of failures, `failures:` then a line
//!   per test indented by 4 spaces, before the binary's summary (`test
//!   result:`). On stderr, the line cargo prints as it starts each binary
//!   (`Running …`, `Doc-tests …`), and the one once the binary has failed,
//!   `error: test failed, to rerun pass `-p foo --lib``, which names the
//!   target. Each stream is in order, but the two only as the daemon read
//!   them, and cargo's line after a binary comes right as it exits, so an
//!   output belongs to the binary at the same place in the other stream
//!   when both count the same binaries, else to the last one cargo started
//!   before the output began. Without cargo's starting lines (`--quiet`),
//!   the outputs that list failures pair with cargo's failure lines in
//!   order when they count the same, else each with the next such line. A
//!   failed target whose binary lists no names (a `harness = false` test)
//!   fails whole; names of no failed target (the run was cut short) belong
//!   to no known target.
//!
//! Colours are no part of a line: escape sequences are dropped first.

use std::io;
use std::sync::LazyLock;

use regex::Regex;

use crate::protocol::Channel;
use crate::runlog::select::Line;

/// A test of nextest's: its binary id (`my-crate`, `my-crate::bin/x`) and
/// its name (`tests::it_works`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextestTest {
    pub binary: String,
    pub name: String,
}

/// A failed test binary of libtest's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The arguments that select it, as cargo said to rerun it (`-p foo
    /// --lib`); `None` when cargo did not say.
    pub selects: Option<Vec<String>>,
    /// The tests that failed in it, by path, in order; empty when libtest
    /// listed none.
    pub names: Vec<String>,
}

/// The failed tests of a nextest run, in the order they first failed.
pub fn nextest(lines: impl Iterator<Item = io::Result<Line>>) -> io::Result<Vec<NextestTest>> {
    let mut tests: Vec<(NextestTest, bool)> = Vec::new();
    for line in lines {
        let line = line?;
        if line.channel != Channel::Stderr {
            continue;
        }
        let text = plain(&line);
        let Some((status, test)) = nextest_status(&text) else {
            continue;
        };
        let words: Vec<&str> = status.split_whitespace().collect();
        let failed = if words.iter().any(|word| matches!(*word, "PASS" | "FLAKY")) {
            false
        } else if words.last().is_some_and(|last| {
            matches!(
                *last,
                "FAIL" | "TIMEOUT" | "ABORT" | "EXECFAIL" | "LEAK-FAIL"
            ) || last.starts_with("SIG")
        }) {
            true
        } else {
            continue;
        };
        match tests.iter_mut().find(|(known, _)| *known == test) {
            Some((_, verdict)) => *verdict = failed,
            None => tests.push((test, failed)),
        }
    }
    Ok(tests
        .into_iter()
        .filter_map(|(test, failed)| failed.then_some(test))
        .collect())
}

/// A status line of nextest's about a test: its status (`FAIL`, `TRY 2
/// FAIL`) and the test. `None` for any other line, among them the
/// summary's and a test's own output, which nextest indents under it.
fn nextest_status(line: &str) -> Option<(&str, NextestTest)> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let (status, rest) = trimmed.split_once(" [")?;
    let status_word = |c: char| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '/');
    if status.is_empty()
        || !status.chars().all(status_word)
        || (indent > 0 && indent + status.len() != 12)
    {
        return None;
    }
    let (time, rest) = rest.split_once("] ")?;
    let time_mark = |c: char| c.is_ascii_digit() || matches!(c, ' ' | '.' | '>' | 's');
    if !time.chars().all(time_mark) {
        return None;
    }
    let rest = match rest.strip_prefix('(') {
        Some(counted) => {
            let (counter, rest) = counted.split_once(") ")?;
            if !counter
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '/' | ' '))
            {
                return None;
            }
            rest
        }
        None => rest,
    };
    let (binary, name) = rest.trim_end().split_once(' ')?;
    if binary.is_empty() || name.is_empty() || name.contains(' ') {
        return None;
    }
    Some((
        status,
        NextestTest {
            binary: binary.to_owned(),
            name: name.to_owned(),
        },
    ))
}

/// A test binary as cargo's stderr shows it.
struct Binary {
    /// The event of the line that started it, or of its hint.
    event: u64,
    /// The target cargo named once it failed.
    selects: Option<Vec<String>>,
}

/// What a test binary printed on stdout, from its `running N tests`.
#[derive(Default)]
struct Output {
    /// The events of its first and last lines.
    event: u64,
    end: u64,
    /// The tests its list of failures names.
    names: Vec<String>,
}

/// The failed test binaries of a `cargo test` run, in the order they
/// failed (see the module docs).
pub fn libtest(lines: impl Iterator<Item = io::Result<Line>>) -> io::Result<Vec<Target>> {
    let mut binaries: Vec<Binary> = Vec::new();
    // Whether cargo said as it started each binary (it does unless
    // `--quiet`).
    let mut started = false;
    let mut outputs: Vec<Output> = Vec::new();
    // The names of the list being read, from its `failures:` on.
    let mut list: Option<Vec<String>> = None;
    let mut listing = false;
    for line in lines {
        let line = line?;
        let text = plain(&line);
        if line.channel == Channel::Stderr {
            if starts_binary(&text) {
                started = true;
                binaries.push(Binary {
                    event: line.event,
                    selects: None,
                });
            } else if let Some(selects) = rerun_hint(&text) {
                match binaries.last_mut() {
                    Some(binary) if started && binary.selects.is_none() => {
                        binary.selects = Some(selects);
                    }
                    _ => binaries.push(Binary {
                        event: line.event,
                        selects: Some(selects),
                    }),
                }
            }
            continue;
        }
        if runs_tests(&text) {
            outputs.push(Output {
                event: line.event,
                end: line.event,
                names: Vec::new(),
            });
            list = None;
            listing = false;
            continue;
        }
        if text == "failures:" {
            list = Some(Vec::new());
            listing = true;
            continue;
        }
        if listing {
            match text.strip_prefix("    ") {
                Some(name) if !name.is_empty() && !name.starts_with(' ') => {
                    if let Some(list) = &mut list {
                        list.push(name.to_owned());
                    }
                    continue;
                }
                _ => listing = false,
            }
        }
        if text.starts_with("test result: ") {
            if outputs.is_empty() {
                outputs.push(Output {
                    event: line.event,
                    ..Output::default()
                });
            }
            let output = outputs.last_mut().expect("BUG: an output was pushed");
            output.names.extend(list.take().unwrap_or_default());
            output.end = line.event;
        }
    }
    // Each output's binary, by its place in each stream when both streams
    // count the same binaries, else by the order of events.
    let outputs: Vec<Output> = if started {
        outputs
    } else {
        outputs
            .into_iter()
            .filter(|output| !output.names.is_empty())
            .collect()
    };
    let in_step = outputs.len() == binaries.len();
    let mut names: Vec<Vec<String>> = binaries.iter().map(|_| Vec::new()).collect();
    let mut unplaced = Vec::new();
    for (place, output) in outputs.into_iter().enumerate() {
        let binary = if in_step {
            Some(place)
        } else if started {
            binaries
                .iter()
                .rposition(|binary| binary.event <= output.event)
        } else {
            binaries
                .iter()
                .position(|binary| binary.event >= output.end)
        };
        match binary {
            Some(binary) => names[binary].extend(output.names),
            None => unplaced.extend(output.names),
        }
    }
    let mut targets = Vec::new();
    for (binary, names) in binaries.into_iter().zip(names) {
        match binary.selects {
            Some(selects) => targets.push(Target {
                selects: Some(selects),
                names,
            }),
            None => unplaced.extend(names),
        }
    }
    if !unplaced.is_empty() {
        targets.push(Target {
            selects: None,
            names: unplaced,
        });
    }
    Ok(targets)
}

/// Whether `line` is cargo's as it starts a test binary, right-aligned to
/// 12 columns: `     Running unittests src/lib.rs (…)`, `   Doc-tests foo`.
fn starts_binary(line: &str) -> bool {
    let word = line.trim_start();
    let indent = line.len() - word.len();
    ["Running ", "Doc-tests "]
        .iter()
        .any(|status| word.starts_with(status) && indent + status.len() == 13)
}

/// Whether `line` is libtest's first of a test binary: `running 3 tests`.
fn runs_tests(line: &str) -> bool {
    line.strip_prefix("running ")
        .and_then(|rest| {
            rest.strip_suffix(" tests")
                .or_else(|| rest.strip_suffix(" test"))
        })
        .is_some_and(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The arguments of cargo's line after a failed test binary, `error: test
/// failed, to rerun pass `-p foo --lib``: `-p`, `foo`, `--lib`.
fn rerun_hint(line: &str) -> Option<Vec<String>> {
    let rest = line.strip_prefix("error: test failed, to rerun pass ")?;
    let quoted = ['`', '\''].iter().find_map(|quote| {
        rest.strip_prefix(*quote)
            .and_then(|rest| rest.strip_suffix(*quote))
    })?;
    Some(quoted.split_whitespace().map(str::to_owned).collect())
}

/// `line` as text, without its line end and escape sequences.
fn plain(line: &Line) -> String {
    static ANSI: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("a constant ANSI pattern"));
    let text = String::from_utf8_lossy(&line.bytes);
    ANSI.replace_all(text.trim_end_matches(['\n', '\r']), "")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of `stdout` and `stderr` pieces, in this order, each piece
    /// an event.
    fn lines(pieces: &[(Channel, &str)]) -> impl Iterator<Item = io::Result<Line>> {
        let lines: Vec<Line> = pieces
            .iter()
            .zip(0..)
            .flat_map(|((channel, text), event)| {
                text.split_inclusive('\n').map(move |line| Line {
                    channel: *channel,
                    event,
                    bytes: line.as_bytes().to_vec(),
                })
            })
            .collect();
        lines.into_iter().map(Ok)
    }

    fn target(selects: &str, names: &[&str]) -> Target {
        Target {
            selects: Some(selects.split(' ').map(str::to_owned).collect()),
            names: names.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn test(binary: &str, name: &str) -> NextestTest {
        NextestTest {
            binary: binary.to_owned(),
            name: name.to_owned(),
        }
    }

    /// Under heli's compact controls, each failure shows twice, once
    /// after the summary, and a test's output under it looks like status
    /// lines; a retry that passes is no failure.
    #[test]
    fn nextest_failures_come_from_its_status_lines() {
        let stderr = "\
    Starting 5 tests across 2 binaries
        FAIL [   0.012s] (1/5) tests-mixed tests::fails
     SIGSEGV [   0.020s] (2/5) many-tests::bin/x g1::t2
  TRY 1 FAIL [   0.001s] (3/5) many-tests flaky
  TRY 2 PASS [   0.001s] (3/5) many-tests flaky
     TIMEOUT [  60.001s] ( 4/15) many-tests g3::t4
────────────
     Summary [   0.329s] 5 tests run: 2 passed, 3 failed, 0 skipped
        FAIL [   0.012s] (1/5) tests-mixed tests::fails
  stdout ───
            FAIL [   0.012s] (9/9) not-a test
        FAIL [   0.012s] (1/5) tests-mixed tests::fails
        PASS [   0.012s] tests-mixed tests::passes
error: test run failed
";
        let failed = nextest(lines(&[(Channel::Stderr, stderr)])).unwrap();
        assert_eq!(
            failed,
            [
                test("tests-mixed", "tests::fails"),
                test("many-tests::bin/x", "g1::t2"),
                test("many-tests", "g3::t4"),
            ]
        );
    }

    #[test]
    fn coloured_status_lines_count() {
        let stderr = "        \x1b[31;1mFAIL\x1b[0m [   0.012s] (1/1) \x1b[35mtests-mixed\x1b[0m \x1b[36mtests::\x1b[0m\x1b[1mfails\x1b[0m\n";
        let failed = nextest(lines(&[(Channel::Stderr, stderr)])).unwrap();
        assert_eq!(failed, [test("tests-mixed", "tests::fails")]);
    }

    /// Each binary's list belongs to the target cargo names after it; its
    /// sections, before the list, are not names. With a binary that prints
    /// no `running` line, the streams count different binaries, and each
    /// output goes to the binary cargo started last before it.
    #[test]
    fn libtest_failures_come_with_cargos_targets() {
        let failed = libtest(lines(&[
            (
                Channel::Stderr,
                "     Running unittests src/lib.rs (target/debug/deps/a-1)\n",
            ),
            (
                Channel::Stdout,
                "\nrunning 3 tests\ntest g1::t2 ... FAILED\n\nfailures:\n\n---- g1::t2 stdout ----\n    indented output\n\nfailures:\n    g1::t2\n    g3::t4\n\ntest result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\n",
            ),
            (
                Channel::Stderr,
                "error: test failed, to rerun pass `-p a --lib`\n     Running tests/it.rs (target/debug/deps/it-2)\n",
            ),
            (Channel::Stdout, "custom harness failed\n"),
            (
                Channel::Stderr,
                "error: test failed, to rerun pass `-p a --test it`\n   Doc-tests a\n",
            ),
            (
                Channel::Stdout,
                "\nrunning 1 test\nfailures:\n    src/lib.rs - f (line 3)\n\ntest result: FAILED. 0 passed; 1 failed\n",
            ),
            (Channel::Stderr, "error: test failed, to rerun pass `-p a --doc`\n"),
        ]))
        .unwrap();
        assert_eq!(
            failed,
            [
                target("-p a --lib", &["g1::t2", "g3::t4"]),
                target("-p a --test it", &[]),
                target("-p a --doc", &["src/lib.rs - f (line 3)"]),
            ]
        );
    }

    /// The daemon may read cargo's line after a failed binary, and the
    /// next binary's start, before the failed binary's last output: the
    /// streams pair by place.
    #[test]
    fn libtest_failures_pair_by_place_when_the_streams_cross() {
        let failed = libtest(lines(&[
            (
                Channel::Stderr,
                "     Running unittests src/lib.rs (a-1)\nerror: test failed, to rerun pass `-p a --lib`\n     Running tests/it.rs (it-2)\n",
            ),
            (
                Channel::Stdout,
                "\nrunning 2 tests\n\nfailures:\n    g1::t2\n\ntest result: FAILED. 1 passed; 1 failed\n\n\nrunning 1 test\n\ntest result: ok. 1 passed; 0 failed\n",
            ),
        ]))
        .unwrap();
        assert_eq!(failed, [target("-p a --lib", &["g1::t2"])]);
    }

    /// Without cargo's starting lines (`--quiet`), the lists of failures
    /// pair with cargo's lines after failed binaries.
    #[test]
    fn quiet_libtest_failures_pair_with_cargos_lines() {
        let failed = libtest(lines(&[
            (
                Channel::Stderr,
                "error: test failed, to rerun pass `-p a --lib`\n",
            ),
            (
                Channel::Stdout,
                "\nrunning 2 tests\n.F\nfailures:\n    g1::t2\n\ntest result: FAILED. 1 passed; 1 failed\n\n\nrunning 1 test\n.\ntest result: ok. 1 passed; 0 failed\n",
            ),
        ]))
        .unwrap();
        assert_eq!(failed, [target("-p a --lib", &["g1::t2"])]);
    }

    #[test]
    fn libtest_failures_without_cargos_word_have_no_target() {
        let failed = libtest(lines(&[(
            Channel::Stdout,
            "failures:\n    g1::t2\n\ntest result: FAILED. 0 passed; 1 failed\n",
        )]))
        .unwrap();
        assert_eq!(
            failed,
            [Target {
                selects: None,
                names: vec!["g1::t2".to_owned()],
            }]
        );
    }
}
