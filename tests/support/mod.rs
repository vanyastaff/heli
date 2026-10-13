//! Shared harness for heli's integration tests.
//!
//! Every test drives the `cargo-heli` binary from outside, as an agent
//! would, against a fixture workspace from `tests/fixtures/` copied into a
//! temporary directory of its own. Commands made here are isolated from the
//! caller's environment: their own target directory, and none of the
//! variables that would change how cargo builds or how many jobs it runs.
//!
//! Fixture `basic` (see its `Cargo.toml`):
//! - `slow-a`, `slow-b`, `slow-c`: build scripts that log their start and end
//!   to `HELI_FIXTURE_LOG` around a sleep of `HELI_FIXTURE_SLEEP_MS`
//!   ([`ConcurrencyLog`]);
//! - `broken`: a compile error; `warnings`: warnings from several lints;
//! - `tests-mixed`: passing, failing and slow tests;
//! - `nested`: a test that runs a nested cargo build;
//! - `echo-args`: prints its arguments and exits with `ECHO_EXIT`.

#![allow(dead_code, reason = "each test binary uses a different part")]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Variables a test command must not inherit: they would move the target
/// directory, change the flags or job count, or hand cargo a jobserver.
const ISOLATED_VARS: [&str; 11] = [
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_MAKEFLAGS",
    "MAKEFLAGS",
    "MFLAGS",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_INCREMENTAL",
];

/// A fixture workspace copied into a temporary directory, with a target
/// directory and a concurrency log beside it.
pub struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// Copies `tests/fixtures/<name>`.
    pub fn new(name: &str) -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        copy_dir(&source, &root.path().join("ws"));
        Self { root }
    }

    /// The copied workspace.
    pub fn workspace(&self) -> PathBuf {
        self.root.path().join("ws")
    }

    /// The target directory every command of this fixture uses.
    pub fn target_dir(&self) -> PathBuf {
        self.root.path().join("target")
    }

    /// `cargo-heli` with `args`, run in the workspace.
    pub fn heli<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command(heli_binary(), args)
    }

    /// Plain cargo with `args` and the same isolation, to compare heli with.
    pub fn cargo<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command(cargo_binary(), args)
    }

    /// `program` with `args`, run in the workspace, isolated.
    pub fn command<I, S>(&self, program: impl Into<OsString>, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut command = Command::new(program.into());
        command
            .args(args.into_iter().map(Into::into))
            .current_dir(self.workspace())
            .env("CARGO", cargo_binary())
            .env("CARGO_TARGET_DIR", self.target_dir())
            .env("CARGO_TERM_COLOR", "never");
        for var in ISOLATED_VARS
            .into_iter()
            .filter(|var| *var != "CARGO_TARGET_DIR")
        {
            command.env_remove(var);
        }
        command
    }

    /// Builds the `echo-args` fixture binary and returns its path.
    pub fn echo_args(&self) -> PathBuf {
        let output = self
            .cargo(["build", "--quiet", "-p", "echo-args"])
            .output()
            .expect("run cargo");
        assert_success(&output, "building echo-args");
        self.target_dir()
            .join("debug")
            .join(format!("echo-args{}", std::env::consts::EXE_SUFFIX))
    }

    /// A concurrency log in this fixture's temporary directory.
    pub fn concurrency_log(&self) -> ConcurrencyLog {
        ConcurrencyLog {
            path: self.root.path().join("concurrency.log"),
        }
    }
}

/// The `cargo-heli` binary under test.
pub fn heli_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-heli"))
}

/// The cargo running the tests, else `cargo` on the search path.
pub fn cargo_binary() -> OsString {
    std::env::var_os("CARGO")
        .filter(|cargo| !cargo.is_empty())
        .unwrap_or_else(|| OsString::from("cargo"))
}

/// Panics with the command's output unless it succeeded.
pub fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: {}\n--- stdout\n{}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The file the fixture build scripts append `<crate> start|end <ns>` lines
/// to; set it on a command with [`ConcurrencyLog::attach`].
pub struct ConcurrencyLog {
    path: PathBuf,
}

/// One build script run: when it started and ended, in nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interval {
    pub name: String,
    pub start: u128,
    pub end: u128,
}

impl ConcurrencyLog {
    /// Makes the fixture build scripts run by `command` log here, each
    /// sleeping `sleep_ms` between its two lines.
    pub fn attach(&self, command: &mut Command, sleep_ms: u64) {
        command
            .env("HELI_FIXTURE_LOG", &self.path)
            .env("HELI_FIXTURE_SLEEP_MS", sleep_ms.to_string());
    }

    /// Every logged run, an `end` paired with the latest open `start` of the
    /// same crate.
    pub fn intervals(&self) -> Vec<Interval> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut open: BTreeMap<&str, Vec<u128>> = BTreeMap::new();
        let mut intervals = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(name), Some(event), Some(at)) = (fields.next(), fields.next(), fields.next())
            else {
                panic!("malformed concurrency log line: {line:?}");
            };
            let at: u128 = at.parse().expect("a timestamp");
            match event {
                "start" => open.entry(name).or_default().push(at),
                "end" => {
                    let start = open
                        .get_mut(name)
                        .and_then(Vec::pop)
                        .unwrap_or_else(|| panic!("`end` without `start` for {name}"));
                    intervals.push(Interval {
                        name: name.to_owned(),
                        start,
                        end: at,
                    });
                }
                other => panic!("unknown event {other:?} in {line:?}"),
            }
        }
        let unfinished: Vec<_> = open
            .iter()
            .filter(|(_, starts)| !starts.is_empty())
            .collect();
        assert!(
            unfinished.is_empty(),
            "runs that never ended: {unfinished:?}"
        );
        intervals
    }

    /// The most runs that were in progress at the same moment.
    pub fn max_concurrency(&self) -> usize {
        let mut events: Vec<(u128, i32)> = self
            .intervals()
            .iter()
            .flat_map(|run| [(run.start, 1), (run.end, -1)])
            .collect();
        // At equal times an end sorts before a start: touching runs do not overlap.
        events.sort();
        let (mut now, mut max) = (0i32, 0i32);
        for (_, delta) in events {
            now += delta;
            max = max.max(now);
        }
        usize::try_from(max).expect("never negative")
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create fixture directory");
    for entry in std::fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}
