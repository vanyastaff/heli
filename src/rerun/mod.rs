//! `cargo heli rerun`: runs a recorded run again (`DESIGN.md` §5,
//! "Reruns").
//!
//! - **What runs.** The run's record starts with its command line as typed
//!   (`crate::runlog::Start`): heli runs it again, from the run's working
//!   directory, with the environment `rerun` is given (the record holds
//!   none). It is a heli run of its own, linked to the one it reruns:
//!   `status` and `log` say `rerun of r17`. Without a run, `rerun` takes the
//!   latest test run in this worktree (`crate::worktree`) whose record shows
//!   failed tests.
//! - **`--failed`**: only the tests that failed ([`failed`]). nextest gets a
//!   filterset of exactly those, `-E '(binary_id(=b) & test(=t)) | …'`
//!   ([`nextest_args`]); libtest gets the failed targets, as cargo named
//!   them, in place of the run's own package and target selection, and the
//!   failed tests' names after `-- --exact` ([`libtest_args`]). When that
//!   cannot be exact, heli says why and runs more: a run that already
//!   selects tests by name, or a target whose failed tests libtest did not
//!   list (`harness = false`), reruns the failed targets whole; doc tests,
//!   which cargo runs with no other target, keep the run's own selection;
//!   a nextest run that already filters, or failures heli cannot place in a
//!   target, rerun the run whole. The first line says what reruns:
//!   `cargo-heli: rerunning 2 failed tests of r17: a::t1, b::t2`.
//! - **`--then-all`**: once the failed tests pass, the whole run again.
//!
//! A run without a start event (it ran before heli recorded them), with a
//! record that has been pruned, or that is not a test run or failed no
//! tests (with `--failed`), is a one-line error, and exit 1.

pub mod failed;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::class::{self, Class, Runner};
use crate::cli::{self, Action};
use crate::locations::Locations;
use crate::run_id::RunId;
use crate::runlog::{self, Start, select::Lines};

use failed::{NextestTest, Target};

/// Exit code when nothing can be rerun.
const FAILED: i32 = 1;

/// `cargo heli rerun [<run>] [--failed [--then-all]]` (see the module docs).
pub fn main(run: Option<RunId>, failed: bool, then_all: bool) -> ! {
    let rerun = plan(run, failed).unwrap_or_else(|reason| {
        eprintln!("cargo-heli: {reason}");
        std::process::exit(FAILED)
    });
    for note in &rerun.notes {
        eprintln!("cargo-heli: note: {note}");
    }
    eprintln!("cargo-heli: {}", rerun.what);
    let code = run_again(rerun.of, &rerun.cwd, &rerun.args);
    if then_all && code == 0 && rerun.args != rerun.whole {
        eprintln!(
            "cargo-heli: the failed tests of {} pass; rerunning it whole",
            rerun.of
        );
        std::process::exit(run_again(rerun.of, &rerun.cwd, &rerun.whole));
    }
    std::process::exit(code)
}

/// What `rerun` runs.
struct Rerun {
    /// The run it reruns.
    of: RunId,
    cwd: PathBuf,
    /// heli's command line for it, after `cargo heli`.
    args: Vec<OsString>,
    /// The command line of the run it reruns, for `--then-all`.
    whole: Vec<OsString>,
    /// What runs, for the first line.
    what: String,
    /// Why more runs than the failed tests.
    notes: Vec<String>,
}

/// Runs heli's command line `args` in `cwd` as a rerun of `of`: this
/// program again, with the current environment. Returns its exit code.
fn run_again(of: RunId, cwd: &Path, args: &[OsString]) -> i32 {
    let status = std::env::current_exe().and_then(|program| {
        Command::new(program)
            .arg("--rerun-of")
            .arg(of.to_string())
            .args(args)
            .current_dir(cwd)
            .status()
    });
    match status {
        Ok(status) => status.code().unwrap_or(FAILED),
        Err(error) => {
            eprintln!("cargo-heli: cannot rerun {of}: {error}");
            FAILED
        }
    }
}

/// A recorded run, read for a rerun.
struct Recorded {
    run: RunId,
    start: Start,
    /// heli's flags, as typed.
    flags: Vec<OsString>,
    /// cargo's arguments, as typed, from the subcommand on; with the test
    /// runner and where its own arguments start, for a test run.
    cargo: Vec<OsString>,
    runner: Option<(Runner, usize)>,
}

impl Recorded {
    /// Reads run `run`'s start event in the run log `dir`.
    fn read(dir: &Path, run: RunId) -> Result<Self, String> {
        let start = runlog::start(dir, run)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!("{run} has no record of its command line: it ran before heli recorded them")
            })?;
        let typed: Vec<OsString> = start.args.iter().map(|arg| arg.0.clone()).collect();
        let parsed = cli::parse(
            std::iter::once(OsString::from("cargo-heli")).chain(typed.iter().cloned()),
            OsString::from("cargo"),
        );
        let cargo = match parsed {
            Ok(Action::Run(invocation)) if !invocation.exec => invocation.args,
            _ => Vec::new(),
        };
        let flags = typed[..typed.len() - cargo.len()].to_vec();
        let runner = (start.class == Class::Test)
            .then(|| class::test_runner(&cargo))
            .flatten();
        Ok(Self {
            run,
            start,
            flags,
            cargo,
            runner,
        })
    }

    fn typed(&self) -> Vec<OsString> {
        self.start.args.iter().map(|arg| arg.0.clone()).collect()
    }

    fn cwd(&self) -> PathBuf {
        PathBuf::from(&self.start.cwd.0)
    }

    /// The tests that failed in it, as its record shows; `None` for a run
    /// that is not a test run.
    fn failures(&self, dir: &Path) -> Result<Option<Failures>, String> {
        let Some((runner, _)) = self.runner else {
            return Ok(None);
        };
        let run = self.run;
        let lines = Lines::new(runlog::open(dir, run).map_err(|error| error.to_string())?);
        let failures = match runner {
            Runner::Nextest => failed::nextest(lines).map(Failures::Nextest),
            Runner::Libtest => failed::libtest(lines).map(Failures::Libtest),
        };
        failures
            .map(Some)
            .map_err(|error| format!("cannot read the record of {run}: {error}"))
    }
}

/// The failed tests of a test run.
enum Failures {
    Nextest(Vec<NextestTest>),
    Libtest(Vec<Target>),
}

impl Failures {
    fn is_empty(&self) -> bool {
        match self {
            Self::Nextest(tests) => tests.is_empty(),
            Self::Libtest(targets) => targets.is_empty(),
        }
    }
}

/// What `rerun` runs for `run` (else the latest failed test run in this
/// worktree), with `failed` only its failed tests; the error says why
/// nothing can.
fn plan(run: Option<RunId>, failed: bool) -> Result<Rerun, String> {
    let dir = Locations::resolve()?.run_logs();
    let (recorded, failures) = match run {
        Some(run) => {
            let recorded = Recorded::read(&dir, run)?;
            let failures = if failed {
                recorded.failures(&dir)?
            } else {
                None
            };
            (recorded, failures)
        }
        None => {
            let (recorded, failures) = latest_failed(&dir)?;
            (recorded, Some(failures))
        }
    };
    let of = recorded.run;
    let whole = recorded.typed();
    let mut rerun = Rerun {
        of,
        cwd: recorded.cwd(),
        args: whole.clone(),
        what: format!("rerunning {of}: cargo heli {}", spaced(&whole)),
        whole,
        notes: Vec::new(),
    };
    if !failed {
        return Ok(rerun);
    }
    let (Some(failures), Some((_, at))) = (failures, recorded.runner) else {
        return Err(format!(
            "{of} is not a test run; `cargo heli rerun {of}` runs it again whole"
        ));
    };
    if failures.is_empty() {
        return Err(format!("{of} has no failed tests to rerun"));
    }
    let cargo = match &failures {
        Failures::Nextest(tests) => match nextest_args(&recorded.cargo, at, tests) {
            Ok(args) => {
                let names: Vec<&str> = tests.iter().map(|test| test.name.as_str()).collect();
                rerun.what = failed_tests(of, &names);
                Some(args)
            }
            Err(reason) => {
                rerun.notes.push(reason);
                None
            }
        },
        Failures::Libtest(targets) => {
            let libtest = libtest_args(&recorded.cargo, at, targets);
            rerun.notes.extend(libtest.notes);
            if let Some(args) = &libtest.args {
                rerun.what = if libtest.by_name {
                    let names: Vec<&str> = targets
                        .iter()
                        .flat_map(|target| target.names.iter().map(String::as_str))
                        .collect();
                    failed_tests(of, &names)
                } else {
                    format!(
                        "rerunning the failed targets of {of}: cargo {}",
                        spaced(args)
                    )
                };
            }
            libtest.args
        }
    };
    if let Some(cargo) = cargo {
        rerun.args = without_package_choice(&recorded.flags)
            .into_iter()
            .chain(cargo)
            .collect();
    }
    Ok(rerun)
}

/// heli's flags `flags` without `--changed` and `--affected`, which would
/// choose packages again from what changed since (and run nothing on an
/// unchanged tree): a rerun of failed tests selects its own.
fn without_package_choice(flags: &[OsString]) -> Vec<OsString> {
    flags
        .iter()
        .filter(|flag| !is_flag(flag, "--changed") && !is_flag(flag, "--affected"))
        .cloned()
        .collect()
}

/// `rerunning 2 failed tests of r17: a::t1, b::t2`.
fn failed_tests(of: RunId, names: &[&str]) -> String {
    let count = names.len();
    format!(
        "rerunning {count} failed test{} of {of}: {}",
        if count == 1 { "" } else { "s" },
        names.join(", ")
    )
}

/// The latest test run in the worktree here whose record shows failed
/// tests, and those.
fn latest_failed(dir: &Path) -> Result<(Recorded, Failures), String> {
    let here = std::env::current_dir()
        .map(|cwd| crate::worktree::of(&cwd))
        .map_err(|error| format!("cannot read the working directory: {error}"))?;
    let mut runs: Vec<RunId> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            name.to_str()?.strip_suffix(".log")?.parse().ok()
        })
        .collect();
    runs.sort_unstable_by_key(|run| std::cmp::Reverse(run.number()));
    for run in runs {
        let Ok(recorded) = Recorded::read(dir, run) else {
            continue;
        };
        if recorded.runner.is_none()
            || !crate::store::same_path(&crate::worktree::of(&recorded.cwd()), &here)
        {
            continue;
        }
        if let Ok(Some(failures)) = recorded.failures(dir)
            && !failures.is_empty()
        {
            return Ok((recorded, failures));
        }
    }
    Err(format!(
        "no test run with failed tests in the run log for {}",
        here.display()
    ))
}

/// `args` as one line, for heli's own lines.
fn spaced(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// nextest's flags that select tests by name.
const NEXTEST_FILTERS: [&str; 3] = ["-E", "--filterset", "--filter-expr"];

/// nextest's flags that take a value in the next argument (checked against
/// nextest 0.9.146), so that a value is not taken for a test filter.
const NEXTEST_VALUES: [&str; 44] = [
    "-E",
    "--filterset",
    "--filter-expr",
    "-p",
    "--package",
    "--exclude",
    "--bin",
    "--example",
    "--test",
    "--bench",
    "-F",
    "--features",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--cargo-profile",
    "--profile",
    "-P",
    "-j",
    "--jobs",
    "--build-jobs",
    "--test-threads",
    "--retries",
    "--flaky-result",
    "--no-tests",
    "--max-fail",
    "--partition",
    "--run-ignored",
    "--status-level",
    "--final-status-level",
    "--success-output",
    "--failure-output",
    "--show-progress",
    "--message-format",
    "--message-format-version",
    "--cargo-message-format",
    "--color",
    "--config",
    "--tool-config-file",
    "--archive-file",
    "--binaries-metadata",
    "--cargo-metadata",
    "--workspace-remap",
    "-Z",
];

/// `cargo <args>`, a nextest run (its own arguments from `at`), narrowed
/// to `tests` with a filterset that names each by binary and name. The
/// error says why it cannot be: the run already filters its tests, which
/// a filterset of heli's would add to (nextest runs a test any filter
/// takes).
fn nextest_args(
    args: &[OsString],
    at: usize,
    tests: &[NextestTest],
) -> Result<Vec<OsString>, String> {
    let own = &args[at..];
    let filters = own.iter().any(|arg| arg == "--")
        || own
            .iter()
            .any(|arg| NEXTEST_FILTERS.iter().any(|flag| is_flag(arg, flag)))
        || has_positional(own, &NEXTEST_VALUES);
    if filters {
        return Err("the run already filters its tests; rerunning it whole".to_owned());
    }
    let filterset = tests
        .iter()
        .map(|test| format!("(binary_id(={}) & test(={}))", test.binary, test.name))
        .collect::<Vec<_>>()
        .join(" | ");
    let mut narrowed = args.to_vec();
    narrowed.extend([OsString::from("-E"), OsString::from(filterset)]);
    Ok(narrowed)
}

/// cargo's flags of `test` that select packages or targets, which the
/// failed targets replace: those that take a value, then switches.
const SELECTION_VALUES: [&str; 7] = [
    "-p",
    "--package",
    "--exclude",
    "--bin",
    "--test",
    "--example",
    "--bench",
];
const SELECTION_SWITCHES: [&str; 9] = [
    "--workspace",
    "--all",
    "--lib",
    "--bins",
    "--tests",
    "--examples",
    "--benches",
    "--all-targets",
    "--doc",
];

/// cargo's flags of `test` that take a value in the next argument, so that
/// a value is not taken for a test name.
const CARGO_TEST_VALUES: [&str; 20] = [
    "-p",
    "--package",
    "--exclude",
    "--bin",
    "--test",
    "--example",
    "--bench",
    "-F",
    "--features",
    "-j",
    "--jobs",
    "--profile",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--message-format",
    "--color",
    "--config",
    "-Z",
    "--lockfile-path",
];

/// libtest's options that take a value in the next argument.
const LIBTEST_VALUES: [&str; 7] = [
    "--test-threads",
    "--skip",
    "--format",
    "--color",
    "--logfile",
    "-Z",
    "--shuffle-seed",
];

/// A libtest rerun's cargo arguments.
#[derive(Debug, PartialEq, Eq)]
struct LibtestArgs {
    /// `None`: the run whole, as it was.
    args: Option<Vec<OsString>>,
    /// Whether they select the failed tests by name.
    by_name: bool,
    /// Why they run more than the failed tests.
    notes: Vec<String>,
}

/// `cargo <args>`, a `cargo test` run (cargo's own arguments of `test` from
/// `at`), narrowed to the failed tests of `targets` (see the module docs).
fn libtest_args(args: &[OsString], at: usize, targets: &[Target]) -> LibtestArgs {
    let separator = args[at..]
        .iter()
        .position(|arg| arg == "--")
        .map_or(args.len(), |position| at + position);
    let own = &args[at..separator];
    let test_args = args.get(separator + 1..).unwrap_or_default();
    let mut notes = Vec::new();
    let mut selections = Vec::new();
    for target in targets {
        match &target.selects {
            Some(selects) => selections.push(selects),
            None => {
                notes.push(format!(
                    "cannot tell which target ran {}; rerunning the run whole",
                    target.names.join(", ")
                ));
                return LibtestArgs {
                    args: None,
                    by_name: false,
                    notes,
                };
            }
        }
    }
    let mut by_name = true;
    if has_positional(own, &CARGO_TEST_VALUES) || has_positional(test_args, &LIBTEST_VALUES) {
        notes.push(
            "the run already selects its tests by name; rerunning its failed targets whole"
                .to_owned(),
        );
        by_name = false;
    }
    for target in targets.iter().filter(|target| target.names.is_empty()) {
        let selects = target.selects.as_deref().unwrap_or_default().join(" ");
        if by_name {
            notes.push(format!(
                "the failed tests of `{selects}` are not listed (a `harness = false` target?); rerunning the failed targets whole"
            ));
        }
        by_name = false;
    }
    let doc = selections.len() > 1
        && selections
            .iter()
            .any(|selects| selects.iter().any(|arg| arg == "--doc"));
    if doc && !by_name {
        notes.push("cargo runs doc tests with no other target; rerunning the run whole".to_owned());
        return LibtestArgs {
            args: None,
            by_name: false,
            notes,
        };
    }
    let mut narrowed = args[..at].to_vec();
    if doc {
        notes.push(
            "cargo runs doc tests with no other target; rerunning the failed tests in all of the run's targets"
                .to_owned(),
        );
        narrowed.extend_from_slice(own);
    } else {
        narrowed.extend(without_selection(own));
        narrowed.extend(selections.into_iter().flatten().map(OsString::from));
    }
    let mut after: Vec<OsString> = test_args.to_vec();
    if by_name {
        if !test_args.iter().any(|arg| arg == "--exact") {
            after.push("--exact".into());
        }
        let mut names: Vec<&String> = targets.iter().flat_map(|target| &target.names).collect();
        let mut seen = std::collections::HashSet::new();
        names.retain(|name| seen.insert(*name));
        after.extend(names.into_iter().map(OsString::from));
    }
    if !after.is_empty() {
        narrowed.push("--".into());
        narrowed.extend(after);
    }
    LibtestArgs {
        args: Some(narrowed),
        by_name,
        notes,
    }
}

/// cargo's arguments of `test`, `args`, without those that select packages
/// or targets.
fn without_selection(args: &[OsString]) -> Vec<OsString> {
    let mut kept = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if SELECTION_SWITCHES.iter().any(|switch| arg == *switch) {
            continue;
        }
        match SELECTION_VALUES
            .iter()
            .find(|flag| is_flag(arg, flag) || joined_short(arg, flag))
        {
            Some(flag) if arg == *flag => {
                args.next();
            }
            Some(_) => {}
            None => kept.push(arg.clone()),
        }
    }
    kept
}

/// Whether `args` hold an argument that is no flag and no flag's value (a
/// test name or filter), `values` being the flags that take one.
fn has_positional(args: &[OsString], values: &[&str]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if values.iter().any(|flag| arg == *flag) {
            args.next();
        } else if !arg.as_encoded_bytes().starts_with(b"-") {
            return true;
        }
    }
    false
}

/// Whether `arg` is `flag`, alone or with its value (`--flag=value`).
fn is_flag(arg: &OsStr, flag: &str) -> bool {
    arg.to_str().is_some_and(|arg| {
        arg.strip_prefix(flag)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
    })
}

/// Whether `arg` is the short `flag` with its value joined (`-pfoo`).
fn joined_short(arg: &OsStr, flag: &str) -> bool {
    !flag.starts_with("--")
        && arg
            .to_str()
            .is_some_and(|arg| arg.len() > flag.len() && arg.starts_with(flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &str) -> Vec<OsString> {
        words.split(' ').map(OsString::from).collect()
    }

    fn words(args: &[OsString]) -> String {
        spaced(args)
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

    #[test]
    fn nextest_reruns_name_each_test_by_binary_and_name() {
        let original = args("nextest run -p many-tests --no-fail-fast");
        let narrowed = nextest_args(
            &original,
            2,
            &[
                test("many-tests", "g1::t2"),
                test("many-tests::it", "g3::t4"),
            ],
        )
        .unwrap();
        assert_eq!(narrowed[..original.len()], original[..]);
        assert_eq!(narrowed[original.len()], "-E");
        assert_eq!(
            narrowed[original.len() + 1],
            "(binary_id(=many-tests) & test(=g1::t2)) | (binary_id(=many-tests::it) & test(=g3::t4))"
        );
        assert_eq!(narrowed.len(), original.len() + 2);
    }

    #[test]
    fn a_rerun_of_failed_tests_chooses_no_packages_from_changes() {
        assert_eq!(
            words(&without_package_choice(&args(
                "--output compact --changed=origin/main --affected --no-cache"
            ))),
            "--output compact --no-cache"
        );
    }

    #[test]
    fn a_nextest_run_that_filters_reruns_whole() {
        let failed = [test("a", "t")];
        for original in [
            "nextest run -E test(=t)",
            "nextest run --filterset=test(=t)",
            "nextest run -p a t",
            "nextest run -- --exact t",
        ] {
            assert!(
                nextest_args(&args(original), 2, &failed).is_err(),
                "{original}"
            );
        }
        assert!(nextest_args(&args("nextest run -p a --retries 2"), 2, &failed).is_ok());
    }

    #[test]
    fn libtest_reruns_select_the_failed_targets_and_names() {
        let narrowed = libtest_args(
            &args("test --workspace --exclude c --release --no-fail-fast"),
            1,
            &[
                target("-p a --lib", &["g1::t2", "g3::t4"]),
                target("-p b --test it", &["it::works"]),
            ],
        );
        assert_eq!(narrowed.notes, Vec::<String>::new());
        assert!(narrowed.by_name);
        assert_eq!(
            words(&narrowed.args.unwrap()),
            "test --release --no-fail-fast -p a --lib -p b --test it -- --exact g1::t2 g3::t4 it::works"
        );
    }

    #[test]
    fn libtest_reruns_keep_the_runs_own_test_options() {
        let narrowed = libtest_args(
            &args("+nightly test -pa --features x -- --test-threads 2 --exact"),
            2,
            &[target("-p a --lib", &["t"])],
        );
        assert_eq!(
            words(&narrowed.args.unwrap()),
            "+nightly test --features x -p a --lib -- --test-threads 2 --exact t"
        );
    }

    #[test]
    fn a_run_that_selects_tests_by_name_reruns_its_failed_targets_whole() {
        for original in ["test -p a g1", "test -p a -- g1 --skip g2"] {
            let narrowed = libtest_args(&args(original), 1, &[target("-p a --lib", &["g1::t2"])]);
            assert!(!narrowed.by_name, "{original}");
            assert_eq!(narrowed.notes.len(), 1, "{original}");
            let narrowed = words(&narrowed.args.unwrap());
            assert!(
                narrowed.contains("-p a --lib") && !narrowed.contains("--exact"),
                "{narrowed}"
            );
        }
    }

    #[test]
    fn an_unlisted_failure_reruns_the_failed_targets_whole() {
        let narrowed = libtest_args(
            &args("test"),
            1,
            &[
                target("-p a --lib", &["t"]),
                target("-p a --test custom", &[]),
            ],
        );
        assert!(!narrowed.by_name);
        assert_eq!(
            words(&narrowed.args.unwrap()),
            "test -p a --lib -p a --test custom"
        );
        assert_eq!(narrowed.notes.len(), 1);
    }

    #[test]
    fn doc_tests_with_other_targets_keep_the_runs_selection() {
        let narrowed = libtest_args(
            &args("test -p a"),
            1,
            &[
                target("-p a --lib", &["t"]),
                target("-p a --doc", &["src/lib.rs - f (line 3)"]),
            ],
        );
        assert!(narrowed.by_name);
        let narrowed = narrowed.args.unwrap();
        assert_eq!(words(&narrowed[..5]), "test -p a -- --exact");
        assert_eq!(narrowed[5], "t");
        assert_eq!(narrowed[6], "src/lib.rs - f (line 3)");
    }

    #[test]
    fn failures_without_a_target_rerun_the_run_whole() {
        let narrowed = libtest_args(
            &args("test"),
            1,
            &[Target {
                selects: None,
                names: vec!["t".to_owned()],
            }],
        );
        assert_eq!(narrowed.args, None);
        assert_eq!(narrowed.notes.len(), 1);
    }
}
