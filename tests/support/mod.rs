//! Shared harness for heli's integration tests.
//!
//! Every test drives the `cargo-heli` binary from outside, as an agent
//! would, against a fixture workspace from `tests/fixtures/` copied into a
//! temporary directory of its own. Commands made here are isolated from the
//! caller's environment: their own target directory, and none of the
//! variables that would change how cargo builds or how many jobs it runs.
//!
//! Each fixture also has a daemon of its own: every command it makes carries
//! `HELI_STATE_DIR` and `HELI_ENDPOINT` inside the fixture's temporary
//! directory, so no test meets another test's daemon or the user's real one.
//! Dropping the fixture kills the daemon, and with it any run still going,
//! even when the test panics. Its host pool (`HELI_POOL`: a semaphore name
//! unique to the fixture on Windows, a FIFO in its temporary directory
//! elsewhere) and its configuration file (`HELI_CONFIG`, absent until
//! [`Fixture::configure`]) are the fixture's own too.
//!
//! Fixture `basic` (see its `Cargo.toml`):
//! - `slow-a`, `slow-b`, `slow-c`: build scripts that log their start and end
//!   to `HELI_FIXTURE_LOG` around a sleep of `HELI_FIXTURE_SLEEP_MS`
//!   ([`ConcurrencyLog`]);
//! - `broken`: a compile error; `warnings`: warnings from several lints;
//! - `tests-mixed`: passing, failing and slow tests;
//! - `nested`: a test that runs a nested cargo build;
//! - `echo-args`: prints its arguments, and on request a variable, its
//!   working directory or its session; waits for a file, leaves an orphan
//!   behind; exits with `ECHO_EXIT` (see its `main.rs`);
//! - `chatter`: numbered lines on stdout and stderr, and on request bytes
//!   that test a run log's fidelity (see its `main.rs`).
//!
//! After every run the daemon executes, heli prints a footer line on stderr
//! ([`split_footer`], [`without_footer`]).

#![allow(dead_code, reason = "each test binary uses a different part")]

pub mod endpoint;
pub mod fake_daemon;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

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

/// heli's variables a test command must not inherit from whoever runs the
/// tests: they could point it at the user's real daemon, pool or
/// configuration.
const HELI_VARS: [&str; 5] = [
    "HELI_STATE_DIR",
    "HELI_ENDPOINT",
    "HELI_DEPTH",
    "HELI_POOL",
    "HELI_CONFIG",
];

/// A fixture workspace copied into a temporary directory, with a target
/// directory, a concurrency log and heli's state directory beside it.
pub struct Fixture {
    root: tempfile::TempDir,
    endpoint: OsString,
    pool: OsString,
    /// Variables set (or, when `None`, removed) on every command, which
    /// select this fixture's daemon.
    isolation: Vec<(OsString, Option<OsString>)>,
}

impl Fixture {
    /// Copies `tests/fixtures/<name>`; its daemon has the state directory
    /// [`Fixture::state_dir`] and the endpoint [`Fixture::endpoint`].
    pub fn new(name: &str) -> Self {
        let mut fixture = Self::bare(name);
        let state_dir = fixture.state_dir();
        let endpoint = fixture.endpoint();
        fixture.isolation = vec![
            ("HELI_STATE_DIR".into(), Some(state_dir.into())),
            ("HELI_ENDPOINT".into(), Some(endpoint)),
            ("HELI_POOL".into(), Some(fixture.pool())),
            ("HELI_CONFIG".into(), Some(fixture.config_file().into())),
        ];
        fixture
    }

    /// Copies `tests/fixtures/<name>`, and leaves heli to find its state
    /// directory and endpoint where it would for a user whose home is
    /// [`Fixture::home`] (`LOCALAPPDATA` and `HOME`), with
    /// `XDG_STATE_HOME` set to `<home>/xdg-state` or unset. Its pool and
    /// configuration follow from that home too (`APPDATA` is the home;
    /// `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR` are unset). Only for
    /// commands that do not need the real home (cargo and rustup live there).
    pub fn with_home(name: &str, xdg_state_home: bool) -> Self {
        let mut fixture = Self::bare(name);
        let home = fixture.home();
        std::fs::create_dir_all(&home).expect("create the temporary home");
        let mut isolation: Vec<(OsString, Option<OsString>)> = HELI_VARS
            .iter()
            .chain(&["XDG_CONFIG_HOME", "XDG_RUNTIME_DIR"])
            .map(|var| (OsString::from(var), None))
            .collect();
        for var in ["LOCALAPPDATA", "APPDATA", "HOME"] {
            isolation.push((var.into(), Some(home.clone().into())));
        }
        let xdg = xdg_state_home.then(|| home.join("xdg-state").into());
        isolation.push(("XDG_STATE_HOME".into(), xdg));
        fixture.isolation = isolation;
        fixture
    }

    fn bare(name: &str) -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        copy_dir(&source, &root.path().join("ws"));
        let endpoint: OsString = if cfg!(windows) {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock after 1970")
                .subsec_nanos();
            format!(
                r"\\.\pipe\heli-test-{}-{}-{nanos}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
            .into()
        } else {
            root.path().join("heli.sock").into()
        };
        let pool = if cfg!(windows) {
            // The pipe's unique name, as a semaphore name.
            let endpoint = endpoint.to_str().expect("an ASCII pipe name");
            endpoint.replace(r"\\.\pipe\", "").into()
        } else {
            root.path().join("jobserver").into()
        };
        Self {
            root,
            endpoint,
            pool,
            isolation: Vec::new(),
        }
    }

    /// The copied workspace.
    pub fn workspace(&self) -> PathBuf {
        self.root.path().join("ws")
    }

    /// A copy of the workspace as it is now, at `name` in the fixture's
    /// temporary directory: a second workspace for the same daemon.
    pub fn copy_workspace(&self, name: &str) -> PathBuf {
        let copy = self.path(name);
        copy_dir(&self.workspace(), &copy);
        copy
    }

    /// The target directory every command of this fixture uses.
    pub fn target_dir(&self) -> PathBuf {
        self.root.path().join("target")
    }

    /// `name` in the fixture's temporary directory, outside the workspace.
    pub fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// The state directory of this fixture's daemon ([`Fixture::new`]).
    pub fn state_dir(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// The temporary home of a [`Fixture::with_home`] fixture.
    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// The endpoint of this fixture's daemon ([`Fixture::new`]): a named
    /// pipe on Windows, a socket in the temporary directory elsewhere.
    pub fn endpoint(&self) -> OsString {
        self.endpoint.clone()
    }

    /// The host pool of this fixture's daemon ([`Fixture::new`]): a
    /// semaphore name on Windows, a FIFO path elsewhere.
    pub fn pool(&self) -> OsString {
        self.pool.clone()
    }

    /// The configuration file of this fixture's daemon ([`Fixture::new`]).
    pub fn config_file(&self) -> PathBuf {
        self.root.path().join("config.toml")
    }

    /// Writes `toml` as the configuration file; a daemon reads it as it
    /// starts.
    pub fn configure(&self, toml: &str) {
        std::fs::write(self.config_file(), toml).expect("write the configuration");
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
            .chain(HELI_VARS)
        {
            command.env_remove(var);
        }
        for (var, value) in &self.isolation {
            match value {
                Some(value) => command.env(var, value),
                None => command.env_remove(var),
            };
        }
        command
    }

    /// `cargo heli status --json`, parsed.
    pub fn status(&self) -> Value {
        let output = self
            .heli(["status", "--json"])
            .output()
            .expect("run heli status");
        assert_success(&output, "cargo heli status --json");
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "status --json is not JSON ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// Polls [`Fixture::status`] until `done` holds, and returns that status;
    /// panics after 30 s, naming `what` it waited for.
    pub fn wait_for_status(&self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.status();
            if done(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; last status: {status}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The runs `status --json` lists.
    pub fn runs(&self) -> Vec<Value> {
        runs(&self.status())
    }

    /// The process id of this fixture's daemon, if one answers.
    pub fn daemon_pid(&self) -> Option<u32> {
        let output = self.heli(["status", "--json"]).output().ok()?;
        let status: Value = serde_json::from_slice(&output.stdout).ok()?;
        daemon_pid(&status)
    }

    /// Kills this fixture's daemon and every process it started, and waits
    /// until it no longer answers. Never panics: it also runs while a failed
    /// test unwinds.
    pub fn kill_daemon(&self) {
        let Some(pid) = self.daemon_pid() else {
            return;
        };
        kill_tree(pid);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.daemon_pid() == Some(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
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

    /// Builds the binary of fixture package `package` and returns its path.
    pub fn binary(&self, package: &str) -> PathBuf {
        let output = self
            .cargo(["build", "--quiet", "-p", package])
            .output()
            .expect("run cargo");
        assert_success(&output, &format!("building {package}"));
        self.target_dir()
            .join("debug")
            .join(format!("{package}{}", std::env::consts::EXE_SUFFIX))
    }

    /// A concurrency log in this fixture's temporary directory.
    pub fn concurrency_log(&self) -> ConcurrencyLog {
        ConcurrencyLog {
            path: self.root.path().join("concurrency.log"),
        }
    }

    /// Another concurrency log, `<name>.log` in this fixture's temporary
    /// directory, for builds whose scripts must be told apart.
    pub fn concurrency_log_named(&self, name: &str) -> ConcurrencyLog {
        ConcurrencyLog {
            path: self.root.path().join(format!("{name}.log")),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}

/// The `runs` of a `status --json` document.
pub fn runs(status: &Value) -> Vec<Value> {
    status["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("status without a runs array: {status}"))
        .clone()
}

/// The daemon's process id in a `status --json` document, if one runs.
pub fn daemon_pid(status: &Value) -> Option<u32> {
    status["daemon"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
}

/// Kills process `pid` and its descendants, found by their parent process
/// (as agent harnesses kill a command), and on Unix its process group: only
/// for processes the tests started themselves.
pub fn kill_tree(pid: u32) {
    let pid = pid.to_string();
    let mut commands = Vec::new();
    if cfg!(windows) {
        let mut taskkill = Command::new("taskkill");
        taskkill.args(["/F", "/T", "/PID", &pid]);
        commands.push(taskkill);
    } else {
        // A daemon leads its own process group, which its runs share.
        let mut group = Command::new("kill");
        group.args(["-KILL", "--", &format!("-{pid}")]);
        let mut children = Command::new("pkill");
        children.args(["-KILL", "-P", &pid]);
        let mut process = Command::new("kill");
        process.args(["-KILL", "--", &pid]);
        commands.extend([group, children, process]);
    }
    for mut command in commands {
        let _ = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Kills process `pid` alone, as a crash would end it: its children live
/// on. Only for processes the tests started themselves.
pub fn kill_process(pid: u32) {
    let pid = pid.to_string();
    let mut command = if cfg!(windows) {
        let mut taskkill = Command::new("taskkill");
        taskkill.args(["/F", "/PID", &pid]);
        taskkill
    } else {
        let mut kill = Command::new("kill");
        kill.args(["-KILL", "--", &pid]);
        kill
    };
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The most of `intervals` in progress at the same moment; touching ones
/// do not overlap.
pub fn max_overlap(intervals: &[Interval]) -> usize {
    let mut events: Vec<(u128, i32)> = intervals
        .iter()
        .flat_map(|run| [(run.start, 1), (run.end, -1)])
        .collect();
    events.sort();
    let (mut now, mut max) = (0i32, 0i32);
    for (_, delta) in events {
        now += delta;
        max = max.max(now);
    }
    usize::try_from(max).expect("never negative")
}

/// The lines of `bytes`, lossily decoded.
pub fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// heli's footer after a run the daemon executed: the last line of the
/// client's stderr, `cargo-heli: <run> … (output: cargo heli log <run>)`.
#[derive(Debug)]
pub struct Footer {
    /// The run it names, `r<number>`.
    pub run: String,
    /// The whole line, without its line end.
    pub line: String,
}

/// Splits a client's stderr into what the run wrote and heli's footer;
/// panics when the last line is not a footer.
pub fn split_footer(stderr: &[u8]) -> (Vec<u8>, Footer) {
    let body = stderr
        .strip_suffix(b"\n")
        .unwrap_or_else(|| panic!("stderr does not end a line: {stderr:?}"));
    let start = body
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |end| end + 1);
    let line = String::from_utf8_lossy(&body[start..]).into_owned();
    let run = line
        .strip_prefix("cargo-heli: ")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|rest| rest.rsplit_once("(output: cargo heli log "))
        .map(|(_, run)| run.to_owned())
        .unwrap_or_else(|| {
            panic!(
                "no footer at the end of stderr: {}",
                String::from_utf8_lossy(stderr)
            )
        });
    (stderr[..start].to_vec(), Footer { run, line })
}

/// A client's stderr without heli's footer, lossily decoded; panics when it
/// has no footer.
pub fn without_footer(stderr: &[u8]) -> String {
    String::from_utf8_lossy(&split_footer(stderr).0).into_owned()
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

    /// How many runs have started and not yet ended, as of now.
    pub fn running(&self) -> usize {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let count = |event: &str| {
            text.lines()
                .filter(|line| line.split_whitespace().nth(1) == Some(event))
                .count()
        };
        count("start").saturating_sub(count("end"))
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
