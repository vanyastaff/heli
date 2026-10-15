//! Test runs (DESIGN.md §5, lifecycle step 6): runs of class test
//! (`crate::class`: `cargo heli test …` and `cargo heli nextest run …`) run
//! in two phases.
//!
//! 1. **Build**: the same command with `--no-run`, right after the
//!    subcommand. For nextest, without what only the test phase reads
//!    ([`RUN_ONLY`], [`RUN_ONLY_SWITCHES`], [`RUN_ONLY_VARS`]): under
//!    `--no-run` nextest ignores those with a warning, or refuses them
//!    (`--no-fail-fast`), and compact output adds some of them
//!    (`crate::output::nextest`).
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

use crate::class::{self, Class, Runner};
use crate::environment;
use crate::protocol::{RunRequest, Text};

/// nextest's flags with a value that only the test phase reads (checked
/// against nextest 0.9.146).
const RUN_ONLY: [&str; 11] = [
    "-j",
    "--test-threads",
    "--jobs",
    "--retries",
    "--flaky-result",
    "--no-tests",
    "--max-fail",
    "--status-level",
    "--final-status-level",
    "--success-output",
    "--failure-output",
];

/// nextest's switches that only the test phase reads; the last two also
/// after `--`.
const RUN_ONLY_SWITCHES: [&str; 4] = [
    "--no-fail-fast",
    "--fail-fast",
    "--no-capture",
    "--nocapture",
];

/// The variables that set [`RUN_ONLY`] flags.
const RUN_ONLY_VARS: [&str; 8] = [
    "NEXTEST_TEST_THREADS",
    "NEXTEST_RETRIES",
    "NEXTEST_FLAKY_RESULT",
    "NEXTEST_NO_TESTS",
    "NEXTEST_STATUS_LEVEL",
    "NEXTEST_FINAL_STATUS_LEVEL",
    "NEXTEST_SUCCESS_OUTPUT",
    "NEXTEST_FAILURE_OUTPUT",
];

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

/// The test run `request` asks for, or `None` for a run of another class
/// (or of none: from a client older than run classes).
pub fn plan(request: &RunRequest) -> Option<TestRun> {
    if request.class != Some(Class::Test) {
        return None;
    }
    let argv = request.argv();
    let (runner, at) = runner(&argv)?;
    let (tool, _) = split_at_separator(&argv[at..]);
    if tool.iter().any(|arg| arg == "--no-run") {
        return None;
    }
    let env = |name: &str| request.var(name)?.to_str().map(str::to_owned);
    let asked = match runner {
        Runner::Nextest => thread_flags(tool, &["-j", "--test-threads", "--jobs"])
            .last()
            .map(|value| nextest_threads(value))
            .or_else(|| env("NEXTEST_TEST_THREADS").map(|value| nextest_threads(&value))),
        Runner::Libtest => {
            let (_, test_args) = split_at_separator(&argv[at..]);
            thread_flags(test_args.unwrap_or_default(), &["--test-threads"])
                .last()
                .map(|value| value.parse().ok().filter(|value: &u32| *value > 0))
                .or_else(|| {
                    env("RUST_TEST_THREADS")
                        .map(|value| value.parse().ok().filter(|value: &u32| *value > 0))
                })
        }
    };
    // Let the runner reject an invalid value itself, with its own output
    // and exit code. Substituting K here would turn a bad invocation green.
    if asked == Some(None) {
        return None;
    }
    let asked = asked.flatten();
    Some(TestRun {
        runner,
        argv,
        at,
        asked,
    })
}

/// Where a test run's flags go (right after `test`, or after `nextest
/// run`) and which tool runs its tests (`crate::class::test_runner`), in
/// `argv`, the program first.
fn runner(argv: &[OsString]) -> Option<(Runner, usize)> {
    let (runner, at) = class::test_runner(argv.get(1..)?)?;
    Some((runner, at + 1))
}

impl TestRun {
    /// The most test threads the user asked for, if any.
    pub fn asked(&self) -> Option<u32> {
        self.asked
    }

    /// The build phase of the run `request` asks for: a request of its own,
    /// with the build's command line and environment (see the module
    /// docs). `None` when the tests cannot be built apart
    /// (`cargo test --doc`).
    pub fn build(&self, request: &RunRequest) -> Option<RunRequest> {
        let (tool, test_args) = split_at_separator(&self.argv[self.at..]);
        let mut argv = self.argv[..self.at].to_vec();
        argv.push("--no-run".into());
        let mut env = request.env.clone();
        match self.runner {
            Runner::Libtest if tool.iter().any(|arg| arg == "--doc") => return None,
            Runner::Libtest => argv.extend(self.argv[self.at..].iter().cloned()),
            Runner::Nextest => {
                let tool = without_flags(tool, &RUN_ONLY);
                argv.extend(without_switches(&tool, &RUN_ONLY_SWITCHES));
                if let Some(test_args) = test_args {
                    argv.push("--".into());
                    argv.extend(without_switches(test_args, &RUN_ONLY_SWITCHES[2..]));
                }
                env.retain(|(name, _)| {
                    !RUN_ONLY_VARS
                        .iter()
                        .any(|var| environment::same_name(&name.0, var))
                });
            }
        }
        Some(RunRequest {
            cwd: request.cwd.clone(),
            argv: argv.into_iter().map(Text).collect(),
            env,
            dependency_epoch: request.dependency_epoch.clone(),
            class: request.class,
            cache: request.cache,
            reads_spawned: request.reads_spawned,
        })
    }

    /// The test phase's command line, with `threads` test threads.
    pub fn test(&self, threads: u32) -> Vec<OsString> {
        let mut argv = self.argv[..self.at].to_vec();
        let (tool, test_args) = split_at_separator(&self.argv[self.at..]);
        match self.runner {
            Runner::Nextest => {
                argv.extend(["--test-threads".into(), threads.to_string().into()]);
                argv.extend(without_flags(tool, &["-j", "--test-threads", "--jobs"]));
                if let Some(test_args) = test_args {
                    argv.push("--".into());
                    argv.extend(test_args.iter().cloned());
                }
            }
            Runner::Libtest => {
                argv.extend(tool.iter().cloned());
                argv.push("--".into());
                argv.extend(without_flags(
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
                values.push(
                    args.next()
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                );
            }
            Flag::Not => {}
        }
    }
    values
}

/// `args` without the flags `names` and their values.
fn without_flags(args: &[OsString], names: &[&str]) -> Vec<OsString> {
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

/// `args` without the switches `names`.
fn without_switches(args: &[OsString], names: &[&str]) -> Vec<OsString> {
    args.iter()
        .filter(|arg| !names.iter().any(|name| *arg == name))
        .cloned()
        .collect()
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
