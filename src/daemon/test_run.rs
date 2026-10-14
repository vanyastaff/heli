//! Test runs (DESIGN.md §5, lifecycle step 6): `cargo heli test …` and
//! `cargo heli nextest run …` run in two phases.
//!
//! 1. **Build**: the same command with `--no-run`, right after the
//!    subcommand.
//! 2. **Test**: the daemon takes K test slots from the pool, then runs the
//!    command again with K threads: `--test-threads K` for nextest (in place
//!    of the user's own), `--test-threads=K` after `--` for libtest.
//!
//! K is at most the threads the user asked for: nextest's `-j`,
//! `--test-threads` or `--jobs`, or `NEXTEST_TEST_THREADS`; libtest's
//! `--test-threads` or `RUST_TEST_THREADS`. A number below 1 for nextest
//! counts from the logical CPUs, as nextest does.
//!
//! Commands that cannot build first run as a test phase only:
//! `cargo test --doc` (cargo refuses `--doc --no-run`). One that already
//! has `--no-run` is a build, not a test run.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::protocol::RunRequest;

/// The tool that runs the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runner {
    /// `cargo test`: libtest, its arguments after `--`.
    Libtest,
    /// `cargo nextest run`.
    Nextest,
}

/// A test run's two command lines.
#[derive(Debug)]
pub struct TestRun {
    runner: Runner,
    /// The command line as the client gave it, program first.
    argv: Vec<OsString>,
    /// Where heli's flags go: right after the subcommand.
    at: usize,
    /// The most test threads the user asked for.
    asked: Option<u32>,
}

/// The test run `request` asks for, or `None` for any other command.
pub fn plan(request: &RunRequest) -> Option<TestRun> {
    let argv: Vec<OsString> = request.argv.iter().map(|arg| arg.0.clone()).collect();
    let (runner, at) = runner(&argv)?;
    let (tool, _) = split_at_separator(&argv[at..]);
    if tool.iter().any(|arg| arg == "--no-run") {
        return None;
    }
    let env = |name: &str| {
        request
            .env
            .iter()
            .rev()
            .find(|(key, _)| same_name(&key.0, name))
            .and_then(|(_, value)| value.0.to_str().map(str::to_owned))
    };
    let asked = match runner {
        Runner::Nextest => thread_flags(tool, &["-j", "--test-threads", "--jobs"])
            .last()
            .map(|value| nextest_threads(value))
            .or_else(|| env("NEXTEST_TEST_THREADS").map(|value| nextest_threads(&value))),
        Runner::Libtest => {
            let (_, test_args) = split_at_separator(&argv[at..]);
            thread_flags(test_args.unwrap_or_default(), &["--test-threads"])
                .last()
                .map(|value| value.parse().ok())
                .or_else(|| env("RUST_TEST_THREADS").map(|value| value.parse().ok()))
        }
    }
    .flatten();
    Some(TestRun {
        runner,
        argv,
        at,
        asked,
    })
}

/// Where a test run's flags go (right after `test`, or after `nextest
/// run`) and which tool runs its tests; `None` for any other command.
///
/// This reads the command line: cargo (a program named `cargo`) with the
/// subcommand `test` (`t`) or `nextest run` (`nextest r`), after an
/// optional `+toolchain`. It is the one place that decides that a run is a
/// test run, until run classes (#12) say so: then a run of class test, with
/// the tool and the position still read here.
fn runner(argv: &[OsString]) -> Option<(Runner, usize)> {
    let program = Path::new(argv.first()?);
    if !program
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo"))
    {
        return None;
    }
    let mut at = 1;
    if argv
        .get(at)
        .is_some_and(|arg| arg.to_string_lossy().starts_with('+'))
    {
        at += 1;
    }
    match argv.get(at)?.to_str()? {
        "test" | "t" => Some((Runner::Libtest, at + 1)),
        "nextest" if matches!(argv.get(at + 1)?.to_str()?, "run" | "r") => {
            Some((Runner::Nextest, at + 2))
        }
        _ => None,
    }
}

impl TestRun {
    /// The most test threads the user asked for, if any.
    pub fn asked(&self) -> Option<u32> {
        self.asked
    }

    /// The build phase's command line, or `None` when the tests cannot be
    /// built apart (`cargo test --doc`).
    pub fn build(&self) -> Option<Vec<OsString>> {
        let (tool, _) = split_at_separator(&self.argv[self.at..]);
        if self.runner == Runner::Libtest && tool.iter().any(|arg| arg == "--doc") {
            return None;
        }
        let mut argv = self.argv.clone();
        argv.insert(self.at, "--no-run".into());
        Some(argv)
    }

    /// The test phase's command line, with `threads` test threads.
    pub fn test(&self, threads: u32) -> Vec<OsString> {
        let mut argv = self.argv[..self.at].to_vec();
        let (tool, test_args) = split_at_separator(&self.argv[self.at..]);
        match self.runner {
            Runner::Nextest => {
                argv.extend(["--test-threads".into(), threads.to_string().into()]);
                argv.extend(without_thread_flags(
                    tool,
                    &["-j", "--test-threads", "--jobs"],
                ));
                if let Some(test_args) = test_args {
                    argv.push("--".into());
                    argv.extend(test_args.iter().cloned());
                }
            }
            Runner::Libtest => {
                argv.extend(tool.iter().cloned());
                argv.push("--".into());
                argv.extend(without_thread_flags(
                    test_args.unwrap_or_default(),
                    &["--test-threads"],
                ));
                argv.push(format!("--test-threads={threads}").into());
            }
        }
        argv
    }
}

/// `args` up to the first `--`, and what follows it, if there is one.
fn split_at_separator(args: &[OsString]) -> (&[OsString], Option<&[OsString]>) {
    match args.iter().position(|arg| arg == "--") {
        Some(separator) => (&args[..separator], Some(&args[separator + 1..])),
        None => (args, None),
    }
}

/// How a thread flag in `arg`, one of `names`, gives its value: in the same
/// argument (`--test-threads=4`, `-j4`), in the next one, or not at all.
enum Flag<'a> {
    Joined(&'a str),
    Next,
    Not,
}

fn flag<'a>(arg: &'a OsStr, names: &[&str]) -> Flag<'a> {
    let Some(arg) = arg.to_str() else {
        return Flag::Not;
    };
    for name in names {
        if arg == *name {
            return Flag::Next;
        }
        let joined = if name.starts_with("--") {
            arg.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
        } else {
            arg.strip_prefix(name).filter(|rest| !rest.is_empty())
        };
        if let Some(value) = joined {
            return Flag::Joined(value);
        }
    }
    Flag::Not
}

/// The values the thread flags `names` have in `args`, in order.
fn thread_flags(args: &[OsString], names: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match flag(arg, names) {
            Flag::Joined(value) => values.push(value.to_owned()),
            Flag::Next => {
                if let Some(value) = args.next() {
                    values.push(value.to_string_lossy().into_owned());
                }
            }
            Flag::Not => {}
        }
    }
    values
}

/// `args` without the thread flags `names` and their values.
fn without_thread_flags(args: &[OsString], names: &[&str]) -> Vec<OsString> {
    let mut kept = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match flag(arg, names) {
            Flag::Joined(_) => {}
            Flag::Next => {
                args.next();
            }
            Flag::Not => kept.push(arg.clone()),
        }
    }
    kept
}

/// The threads a nextest value asks for: a positive number, or the logical
/// CPUs (`num-cpus`), less the amount of a number below 1. `None` for a
/// value nextest refuses.
fn nextest_threads(value: &str) -> Option<u32> {
    let cpus = std::thread::available_parallelism()
        .map_or(1, |cpus| i64::try_from(cpus.get()).unwrap_or(i64::MAX));
    let threads = match value {
        "num-cpus" => cpus,
        value => {
            let number: i64 = value.parse().ok()?;
            if number >= 1 { number } else { cpus + number }
        }
    };
    u32::try_from(threads.max(1)).ok()
}

#[cfg(windows)]
fn same_name(name: &OsStr, other: &str) -> bool {
    name.eq_ignore_ascii_case(other)
}

#[cfg(not(windows))]
fn same_name(name: &OsStr, other: &str) -> bool {
    name == other
}
