//! The result store (DESIGN.md §7): a verify or test run that already passed
//! with exactly the same inputs is not run again; heli says so and exits 0.

mod support;

use std::process::{Command, Output};

use support::{Fixture, assert_success, lines};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `git <args>` in the fixture's workspace; panics unless it succeeds, and
/// returns its stdout.
fn git(fixture: &Fixture, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(fixture.workspace())
        .output()
        .expect("run git");
    assert_success(&output, &format!("git {}", args.join(" ")));
    text(&output.stdout)
}

/// Makes the fixture's workspace a git repository with one commit of all its
/// files, `Cargo.lock` included; `ignored/` is ignored.
fn init_repo(fixture: &Fixture) {
    std::fs::write(fixture.workspace().join(".gitignore"), "/ignored/\n")
        .expect("write .gitignore");
    let lockfile = fixture
        .cargo(["generate-lockfile", "--offline"])
        .output()
        .expect("run cargo");
    assert_success(&lockfile, "cargo generate-lockfile");
    git(fixture, &["init", "--quiet"]);
    git(fixture, &["add", "-A"]);
    git(
        fixture,
        &[
            "-c",
            "user.name=heli",
            "-c",
            "user.email=heli@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--no-verify",
            "-m",
            "fixture",
        ],
    );
}

/// The line that reports a hit, if `output` is one: exit 0, nothing on
/// stdout, and `already passed` on stderr.
fn hit_line(output: &Output) -> Option<String> {
    let line = lines(&output.stderr)
        .into_iter()
        .find(|line| line.starts_with("cargo-heli: already passed: "))?;
    assert_eq!(output.status.code(), Some(0), "a hit exits 0");
    assert!(output.stdout.is_empty(), "a hit runs nothing: {output:?}");
    Some(line)
}

/// How a run that exited 0 went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The result store had it: nothing ran.
    Hit,
    /// The command ran.
    Ran,
}

/// A fixture repository whose `cargo` is a copy of `echo-args` outside the
/// worktree: it passes whatever it is asked, and answers version requests
/// with the contents of [`Verifier::version`].
struct Verifier {
    fixture: Fixture,
    cargo: std::path::PathBuf,
}

impl Verifier {
    fn new() -> Self {
        let fixture = Fixture::new("basic");
        let echo = fixture.echo_args();
        init_repo(&fixture);
        let dir = fixture.path("fake-cargo");
        std::fs::create_dir_all(&dir).expect("create the fake cargo's directory");
        let cargo = dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(&echo, &cargo).expect("copy echo-args");
        let verifier = Self { fixture, cargo };
        std::fs::write(verifier.version(), "cargo 1.0.0").expect("write the version");
        verifier
    }

    /// The file whose contents the fake cargo reports as its version.
    fn version(&self) -> std::path::PathBuf {
        self.fixture.path("cargo-version")
    }

    /// `cargo heli <args>` with the fake cargo.
    fn heli(&self, args: &[&str]) -> Command {
        let mut command = self.fixture.heli(args.iter().copied());
        command
            .env("CARGO", &self.cargo)
            .env("ECHO_VERSION", self.version());
        command
    }

    /// `cargo heli check`.
    fn check(&self) -> Command {
        self.heli(&["check"])
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.fixture.workspace().join(relative)
    }
}

/// Runs `command`, which must exit 0, and tells whether it was a hit.
fn outcome(command: &mut Command) -> Outcome {
    let output = command.output().expect("run heli");
    assert_success(&output, "cargo heli");
    if hit_line(&output).is_some() {
        Outcome::Hit
    } else {
        assert!(
            text(&output.stdout).starts_with("arg: "),
            "the command ran: {output:?}"
        );
        Outcome::Ran
    }
}

/// One change to a run's inputs: `apply` makes it and returns the command
/// to run, `revert` undoes it.
struct Change {
    name: &'static str,
    apply: fn(&Verifier) -> Command,
    revert: fn(&Verifier),
}

/// Asserts that each change is a miss, and that once it is reverted the
/// recorded run is a hit again.
fn assert_each_misses(verifier: &Verifier, changes: &[Change]) {
    assert_eq!(
        outcome(&mut verifier.check()),
        Outcome::Ran,
        "the first run"
    );
    assert_eq!(
        outcome(&mut verifier.check()),
        Outcome::Hit,
        "the same inputs"
    );
    let mut hits = Vec::new();
    for change in changes {
        if outcome(&mut (change.apply)(verifier)) == Outcome::Hit {
            hits.push(change.name);
        }
        (change.revert)(verifier);
        assert_eq!(
            outcome(&mut verifier.check()),
            Outcome::Hit,
            "after reverting {}",
            change.name
        );
    }
    assert!(hits.is_empty(), "changes that still hit: {hits:?}");
}

fn nothing(_: &Verifier) {}

/// A change to the worktree's content (a tracked file, an untracked one),
/// the arguments or the directory the run starts in, the toolchain or the
/// Cargo configuration is a miss.
#[test]
fn a_change_to_a_keyed_input_misses() {
    let verifier = Verifier::new();
    assert_each_misses(
        &verifier,
        &[
            Change {
                name: "a tracked file",
                apply: |verifier| {
                    let path = verifier.path("crates/slow-a/src/lib.rs");
                    let mut text = std::fs::read_to_string(&path).expect("read");
                    text.push_str("// edited\n");
                    std::fs::write(&path, text).expect("write");
                    verifier.check()
                },
                revert: |verifier| {
                    git(
                        &verifier.fixture,
                        &["checkout", "--", "crates/slow-a/src/lib.rs"],
                    );
                },
            },
            Change {
                name: "an untracked file",
                apply: |verifier| {
                    std::fs::write(verifier.path("crates/new.txt"), "new").expect("write");
                    verifier.check()
                },
                revert: |verifier| {
                    std::fs::remove_file(verifier.path("crates/new.txt")).expect("remove");
                },
            },
            Change {
                name: "the arguments",
                apply: |verifier| verifier.heli(&["check", "--all-targets"]),
                revert: nothing,
            },
            Change {
                name: "the working directory",
                apply: |verifier| {
                    let mut command = verifier.check();
                    command.current_dir(verifier.path("crates"));
                    command
                },
                revert: nothing,
            },
            Change {
                name: "the toolchain",
                apply: |verifier| {
                    std::fs::write(verifier.version(), "cargo 2.0.0").expect("write");
                    verifier.check()
                },
                revert: |verifier| {
                    std::fs::write(verifier.version(), "cargo 1.0.0").expect("write");
                },
            },
            Change {
                name: "the Cargo configuration above the worktree",
                apply: |verifier| {
                    let dir = verifier.fixture.path(".cargo");
                    std::fs::create_dir_all(&dir).expect("create .cargo");
                    std::fs::write(dir.join("config.toml"), "[build]\nincremental = false\n")
                        .expect("write");
                    verifier.check()
                },
                revert: |verifier| {
                    std::fs::remove_dir_all(verifier.fixture.path(".cargo")).expect("remove");
                },
            },
        ],
    );
}

/// The search path with `entry` first.
fn path_with(entry: impl Into<std::path::PathBuf>) -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(entry.into()).chain(std::env::split_paths(&path)))
        .expect("a search path")
}

/// A value no Unicode text spells, distinct for each `n`: a lossy
/// conversion turns them all into one replacement character.
#[cfg(unix)]
fn unspellable(n: u8) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(vec![b'x', 0x80 + n])
}

#[cfg(windows)]
fn unspellable(n: u8) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xD800 + u16::from(n)])
}

/// Every variable is keyed but the neutral ones: a test control, a variable
/// nobody listed, a job or thread count the tool would reject, and a search
/// path entry outside the worktree are misses; the neutral variables, valid
/// counts and search path entries inside the worktree (cargo prepends its
/// output directories) are not.
#[test]
fn every_variable_but_the_neutral_ones_is_keyed() {
    let verifier = Verifier::new();
    assert_eq!(outcome(&mut verifier.check()), Outcome::Ran);
    let outside = verifier.fixture.path("tools");
    let inside = verifier.path("target-elsewhere/debug/deps");
    let keyed: Vec<(&str, std::ffi::OsString)> = vec![
        ("PROPTEST_CASES", "100000".into()),
        ("RESULTS_TEST_UNLISTED", "1".into()),
        ("RUSTFLAGS", "-Dwarnings".into()),
        ("CARGO_BUILD_JOBS", "0".into()),
        ("CARGO_BUILD_JOBS", "many".into()),
        ("NEXTEST_TEST_THREADS", "0".into()),
        ("RUST_TEST_THREADS", "-1".into()),
        ("PATH", path_with(&outside)),
    ];
    let neutral: Vec<(&str, std::ffi::OsString)> = vec![
        ("CARGO_BUILD_JOBS", "28".into()),
        ("CARGO_BUILD_JOBS", "-2".into()),
        ("CARGO_BUILD_JOBS", "default".into()),
        ("NEXTEST_TEST_THREADS", "num-cpus".into()),
        ("RUST_TEST_THREADS", "3".into()),
        ("CARGO_MAKEFLAGS", "-j --jobserver-auth=heli-test".into()),
        ("HELI_RESULTS_TEST", "1".into()),
        ("CARGO_TERM_COLOR", "always".into()),
        ("CARGO_PKG_VERSION", "9.9.9".into()),
        ("CARGO_MANIFEST_DIR", verifier.path("crates").into()),
        (
            "CARGO_TARGET_DIR",
            verifier.fixture.path("other-target").into(),
        ),
        ("SHLVL", "7".into()),
        ("CLAUDE_CODE_SESSION_ID", "0f1e".into()),
        ("PATH", path_with(&inside)),
    ];
    let unkeyed: Vec<&str> = keyed
        .iter()
        .filter(|(name, value)| outcome(verifier.check().env(name, value)) == Outcome::Hit)
        .map(|(name, _)| *name)
        .collect();
    assert!(unkeyed.is_empty(), "not keyed: {unkeyed:?}");
    let missed: Vec<&str> = neutral
        .iter()
        .filter(|(name, value)| outcome(verifier.check().env(name, value)) == Outcome::Ran)
        .map(|(name, _)| *name)
        .collect();
    assert!(missed.is_empty(), "neutral but keyed: {missed:?}");
}

/// Values are keyed as raw bytes, never as lossy text: two variables, two
/// search paths or two Cargo configurations that differ only in what Unicode
/// cannot spell are different keys.
#[test]
fn values_are_keyed_as_raw_bytes() {
    let verifier = Verifier::new();
    for name in ["RESULTS_TEST_ODD", "PATH"] {
        let value = |n| match name {
            "PATH" => path_with(unspellable(n)),
            _ => unspellable(n),
        };
        assert_eq!(outcome(verifier.check().env(name, value(0))), Outcome::Ran);
        assert_eq!(outcome(verifier.check().env(name, value(0))), Outcome::Hit);
        assert_eq!(
            outcome(verifier.check().env(name, value(1))),
            Outcome::Ran,
            "{name}"
        );
    }
    let config = verifier.fixture.path(".cargo");
    std::fs::create_dir_all(&config).expect("create .cargo");
    for byte in [0xFF, 0xFE] {
        std::fs::write(config.join("config"), [b'#', b' ', byte, b'\n']).expect("write");
        assert_eq!(
            outcome(&mut verifier.check()),
            Outcome::Ran,
            "configuration byte {byte:#x}"
        );
    }
}

/// Each variable is its own field: two variables whose names and values
/// run together into the same text are different keys.
#[test]
fn variables_that_run_together_are_different_keys() {
    let verifier = Verifier::new();
    assert_eq!(
        outcome(verifier.check().env("RESULTS_TEST_AB", "C")),
        Outcome::Ran
    );
    assert_eq!(
        outcome(verifier.check().env("RESULTS_TEST_A", "BC")),
        Outcome::Ran
    );
}

/// A variable whose name differs only in case is another variable on Unix,
/// and the same one on Windows, where names ignore case.
#[test]
fn the_case_of_a_name_counts_on_unix_only() {
    let verifier = Verifier::new();
    assert_eq!(
        outcome(verifier.check().env("RESULTS_TEST_CASE", "1")),
        Outcome::Ran
    );
    let expected = if cfg!(windows) {
        Outcome::Hit
    } else {
        Outcome::Ran
    };
    assert_eq!(
        outcome(verifier.check().env("results_test_case", "1")),
        expected
    );
}

/// Ignored files are not part of the key (one of the caveats a hit states).
#[test]
fn an_ignored_file_is_not_part_of_the_key() {
    let verifier = Verifier::new();
    assert_eq!(outcome(&mut verifier.check()), Outcome::Ran);
    std::fs::create_dir_all(verifier.path("ignored")).expect("create ignored/");
    std::fs::write(verifier.path("ignored/file"), "x").expect("write");
    assert_eq!(outcome(&mut verifier.check()), Outcome::Hit);
}

/// A passing `cargo heli check` run again with the same inputs is a hit: it
/// names the run that passed and when, states the caveats, exits 0 and runs
/// nothing.
#[test]
fn the_same_inputs_give_a_hit() {
    let fixture = Fixture::new("basic");
    init_repo(&fixture);
    let check = || {
        fixture
            .heli(["check", "-p", "slow-a"])
            .output()
            .expect("run heli")
    };

    let first = check();
    assert_success(&first, "the first check");
    assert!(hit_line(&first).is_none(), "{}", text(&first.stderr));
    assert!(
        text(&first.stderr).contains("Finished"),
        "cargo ran: {}",
        text(&first.stderr)
    );

    let second = check();
    let hit = hit_line(&second).unwrap_or_else(|| panic!("not a hit: {}", text(&second.stderr)));
    assert!(hit.starts_with("cargo-heli: already passed: r1, "), "{hit}");
    assert!(hit.contains(" ago"), "says when: {hit}");
    let stderr = text(&second.stderr);
    assert!(!stderr.contains("Finished"), "cargo did not run: {stderr}");
    for caveat in ["flaky", "ignored files", "--no-cache"] {
        assert!(stderr.contains(caveat), "{caveat:?} missing: {stderr}");
    }
}
