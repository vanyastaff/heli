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

/// The versions the fake tools report at first, by request.
const VERSIONS: [(&str, &str); 5] = [
    ("cargo", "cargo 1.0.0"),
    ("rustc", "rustc 1.0.0"),
    ("cargo clippy", "clippy 1.0.0"),
    ("cargo fmt", "rustfmt 1.0.0"),
    ("cargo nextest", "cargo-nextest 1.0.0"),
];

/// A fixture repository whose toolchain is fake: its `cargo` is a copy of
/// `echo-args` outside the worktree, which passes whatever it is asked, and
/// its `rustc` another, in an ignored directory of the worktree on the search
/// path (so neither the tree nor the search path's key sees it). Each answers
/// version requests as [`Verifier::set_version`] sets them. Its Cargo home
/// is an empty directory of its own.
struct Verifier {
    fixture: Fixture,
    echo: std::path::PathBuf,
    cargo: std::path::PathBuf,
}

impl Verifier {
    fn new() -> Self {
        let fixture = Fixture::new("basic");
        let echo = fixture.echo_args();
        init_repo(&fixture);
        let mut verifier = Self {
            fixture,
            echo,
            cargo: std::path::PathBuf::new(),
        };
        verifier.cargo = verifier.tool(&verifier.fixture.path("fake-cargo"), "cargo");
        verifier.tool(&verifier.fake_bin(), "rustc");
        std::fs::create_dir_all(verifier.fixture.path("versions")).expect("create versions");
        std::fs::create_dir_all(verifier.cargo_home()).expect("create the Cargo home");
        for (request, version) in VERSIONS {
            verifier.set_version(request, Some(version));
        }
        verifier
    }

    /// Copies `echo-args` into `dir` as the fake tool `name`.
    fn tool(&self, dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).expect("create a fake tool's directory");
        let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(&self.echo, &path).expect("copy echo-args");
        path
    }

    /// The Cargo home every run has (`CARGO_HOME`).
    fn cargo_home(&self) -> std::path::PathBuf {
        self.fixture.path("cargo-home")
    }

    /// Sets the version the fake tools report for `request` (`cargo`,
    /// `cargo clippy`, `rustc`); `None` makes the request fail.
    fn set_version(&self, request: &str, version: Option<&str>) {
        let path = self.fixture.path("versions").join(request);
        match version {
            Some(version) => std::fs::write(path, version).expect("write a version"),
            None => std::fs::remove_file(path).expect("remove a version"),
        }
    }

    /// The directory of the fake `rustc`: ignored, inside the worktree.
    fn fake_bin(&self) -> std::path::PathBuf {
        self.path("ignored/bin")
    }

    /// The search path with `entry` first, then the fake `rustc`'s
    /// directory, then the real search path.
    fn path_with(&self, entry: impl Into<std::path::PathBuf>) -> std::ffi::OsString {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let entries = [entry.into(), self.fake_bin()];
        std::env::join_paths(entries.into_iter().chain(std::env::split_paths(&path)))
            .expect("a search path")
    }

    /// `cargo heli <args>` with the fake toolchain.
    fn heli(&self, args: &[&str]) -> Command {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.fake_bin()).chain(std::env::split_paths(&path)),
        )
        .expect("a search path");
        let mut command = self.fixture.heli(args.iter().copied());
        command
            .env("CARGO", &self.cargo)
            .env("CARGO_HOME", self.cargo_home())
            .env("ECHO_VERSION", self.fixture.path("versions"))
            .env("PATH", path);
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
                name: "rustc's version",
                apply: |verifier| {
                    verifier.set_version("rustc", Some("rustc 2.0.0"));
                    verifier.check()
                },
                revert: |verifier| verifier.set_version("rustc", Some("rustc 1.0.0")),
            },
            Change {
                name: "cargo's version",
                apply: |verifier| {
                    verifier.set_version("cargo", Some("cargo 2.0.0"));
                    verifier.check()
                },
                revert: |verifier| verifier.set_version("cargo", Some("cargo 1.0.0")),
            },
            Change {
                name: "the Cargo configuration in the Cargo home",
                apply: |verifier| {
                    std::fs::write(
                        verifier.cargo_home().join("config.toml"),
                        "[build]\nincremental = false\n",
                    )
                    .expect("write");
                    verifier.check()
                },
                revert: |verifier| {
                    std::fs::remove_file(verifier.cargo_home().join("config.toml"))
                        .expect("remove");
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

/// The search path `name` as the tests inherit it (rustup's proxy, for one,
/// sets the library path to its toolchain), with `entry` first.
fn prepended(name: &str, entry: &std::path::Path) -> std::ffi::OsString {
    let inherited = std::env::var_os(name).unwrap_or_default();
    let entries = std::iter::once(entry.to_path_buf())
        .chain(std::env::split_paths(&inherited).filter(|path| !path.as_os_str().is_empty()));
    std::env::join_paths(entries).expect("a search path")
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
    // As the client sees its worktree: its working directory is resolved
    // (on macOS the temporary directory is behind a symbolic link), and so
    // are the entries cargo derives from it.
    let workspace = if cfg!(unix) {
        std::fs::canonicalize(verifier.fixture.workspace()).expect("the workspace")
    } else {
        verifier.fixture.workspace()
    };
    let inside = workspace
        .join("target-elsewhere")
        .join("debug")
        .join("deps");
    let mut sibling = workspace.clone().into_os_string();
    sibling.push("-sibling");
    let other_home = verifier.fixture.path("other-cargo-home");
    std::fs::create_dir_all(&other_home).expect("create another Cargo home");
    // The same fake cargo elsewhere: the program is not keyed, its versions
    // are.
    let other_cargo = verifier.tool(&verifier.fixture.path("other-cargo"), "cargo");
    let keyed: Vec<(&str, std::ffi::OsString)> = vec![
        ("PROPTEST_CASES", "100000".into()),
        ("RESULTS_TEST_UNLISTED", "1".into()),
        ("RUSTFLAGS", "-Dwarnings".into()),
        ("MAKEFLAGS", "-n".into()),
        ("CARGO_BUILD_JOBS", "0".into()),
        ("CARGO_BUILD_JOBS", "many".into()),
        ("NEXTEST_TEST_THREADS", "0".into()),
        ("NEXTEST_TEST_THREADS", "all".into()),
        ("RUST_TEST_THREADS", "0".into()),
        ("RUST_TEST_THREADS", "-1".into()),
        ("NUMBER_OF_PROCESSORS", "0".into()),
        ("PATH", verifier.path_with(&outside)),
        ("PATH", verifier.path_with(&sibling)),
        ("LD_LIBRARY_PATH", outside.clone().into()),
    ];
    let mut neutral: Vec<(&str, std::ffi::OsString)> = vec![
        ("CARGO_BUILD_JOBS", "28".into()),
        ("CARGO_BUILD_JOBS", "-2".into()),
        ("CARGO_BUILD_JOBS", "default".into()),
        ("NEXTEST_TEST_THREADS", "16".into()),
        ("NEXTEST_TEST_THREADS", "-1".into()),
        ("NEXTEST_TEST_THREADS", "num-cpus".into()),
        ("RUST_TEST_THREADS", "3".into()),
        ("NUMBER_OF_PROCESSORS", "32".into()),
        ("CARGO_MAKEFLAGS", "-j --jobserver-auth=heli-test".into()),
        ("HELI_RESULTS_TEST", "1".into()),
        ("CARGO_TERM_COLOR", "always".into()),
        ("CARGO", other_cargo.into()),
        ("CARGO_MANIFEST_DIR", verifier.path("crates").into()),
        ("CARGO_MANIFEST_PATH", verifier.path("Cargo.toml").into()),
        ("CARGO_PKG_VERSION", "9.9.9".into()),
        ("CARGO_CRATE_NAME", "results_test".into()),
        ("CARGO_BIN_NAME", "results-test".into()),
        ("CARGO_BIN_EXE_results-test", "results-test".into()),
        ("CARGO_PRIMARY_PACKAGE", "1".into()),
        ("CARGO_RUSTC_CURRENT_DIR", verifier.path("crates").into()),
        ("CARGO_TARGET_TMPDIR", verifier.fixture.path("tmp").into()),
        ("CARGO_HOME", other_home.into()),
        (
            "CARGO_TARGET_DIR",
            verifier.fixture.path("other-target").into(),
        ),
        ("RUSTUP_TOOLCHAIN", "results-test".into()),
        ("PWD", verifier.path("crates").into()),
        ("OLDPWD", verifier.path("crates").into()),
        ("SHLVL", "7".into()),
        ("_", "/usr/bin/env".into()),
        ("WT_SESSION", "0f1e".into()),
        ("WT_PROFILE_ID", "0f1e".into()),
        ("TERM_SESSION_ID", "0f1e".into()),
        ("ITERM_SESSION_ID", "0f1e".into()),
        ("TMUX_PANE", "%1".into()),
        ("WINDOWID", "42".into()),
        ("XDG_SESSION_ID", "3".into()),
        ("SECURITYSESSIONID", "186a6".into()),
        ("CLAUDECODE", "1".into()),
        ("CLAUDE_CODE_SESSION_ID", "0f1e".into()),
        ("CHROME_CRASHPAD_PIPE_NAME", "0f1e".into()),
        ("PATH", verifier.path_with(&inside)),
        ("PATH", verifier.path_with(&workspace)),
        ("LD_LIBRARY_PATH", prepended("LD_LIBRARY_PATH", &inside)),
        ("DYLD_LIBRARY_PATH", prepended("DYLD_LIBRARY_PATH", &inside)),
        (
            "DYLD_FALLBACK_LIBRARY_PATH",
            prepended("DYLD_FALLBACK_LIBRARY_PATH", &inside),
        ),
        (
            "DYLD_FRAMEWORK_PATH",
            prepended("DYLD_FRAMEWORK_PATH", &inside),
        ),
        (
            "DYLD_FALLBACK_FRAMEWORK_PATH",
            prepended("DYLD_FALLBACK_FRAMEWORK_PATH", &inside),
        ),
    ];
    if cfg!(windows) {
        // Windows paths ignore case, and take either separator.
        let spelled = inside.to_string_lossy().to_uppercase().replace('\\', "/");
        neutral.push(("PATH", verifier.path_with(spelled)));
    }
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
            "PATH" => verifier.path_with(unspellable(n)),
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

/// The version of the tool a run uses (clippy, rustfmt, nextest) is keyed
/// for the runs of that tool only, a missing tool as absent.
#[test]
fn the_tool_a_run_uses_is_keyed() {
    let verifier = Verifier::new();
    assert_eq!(outcome(&mut verifier.check()), Outcome::Ran);
    for (args, request, version) in [
        (&["clippy"][..], "cargo clippy", "clippy 1.0.0"),
        (&["fmt", "--check"][..], "cargo fmt", "rustfmt 1.0.0"),
        (
            &["nextest", "run"][..],
            "cargo nextest",
            "cargo-nextest 1.0.0",
        ),
    ] {
        let run = || outcome(&mut verifier.heli(args));
        assert_eq!(run(), Outcome::Ran, "{args:?}");
        assert_eq!(run(), Outcome::Hit, "{args:?}");
        verifier.set_version(request, Some("upgraded"));
        assert_eq!(run(), Outcome::Ran, "{request} upgraded");
        assert_eq!(
            outcome(&mut verifier.check()),
            Outcome::Hit,
            "check does not run {request}"
        );
        verifier.set_version(request, None);
        assert_eq!(run(), Outcome::Ran, "{request} missing");
        assert_eq!(run(), Outcome::Hit, "{request} still missing");
        verifier.set_version(request, Some(version));
        assert_eq!(run(), Outcome::Hit, "{request} back");
    }
}

/// Verify and test runs use the store (`check`, `clippy`, `fmt --check`,
/// `test`, `nextest run`); compile, mutate and other runs, and requests for
/// help, do not.
#[test]
fn verify_and_test_runs_use_the_store() {
    let verifier = Verifier::new();
    let cached: [&[&str]; 10] = [
        &["check"],
        &["c"],
        &["clippy", "--", "-Dwarnings"],
        &["fmt", "--check"],
        &["fmt", "--", "--check"],
        &["test"],
        &["t"],
        &["nextest", "run"],
        &["nextest", "r"],
        &["+results-test", "check"],
    ];
    let uncached: [&[&str]; 9] = [
        &["build"],
        &["run"],
        &["doc"],
        &["clippy", "--fix"],
        &["fmt"],
        &["fix"],
        &["nextest", "list"],
        &["check", "--help"],
        &["test", "--", "--help"],
    ];
    let twice = |args: &[&str]| {
        outcome(&mut verifier.heli(args));
        outcome(&mut verifier.heli(args))
    };
    let missed: Vec<_> = cached
        .into_iter()
        .filter(|args| twice(args) == Outcome::Ran)
        .collect();
    assert!(missed.is_empty(), "not cached: {missed:?}");
    let hit: Vec<_> = uncached
        .into_iter()
        .filter(|args| twice(args) == Outcome::Hit)
        .collect();
    assert!(hit.is_empty(), "cached: {hit:?}");
}

/// A toolchain given as `+toolchain` before the subcommand is the one asked
/// for its versions: rustc's, cargo's and the tool's.
#[test]
fn a_toolchain_given_with_plus_is_the_one_asked() {
    let verifier = Verifier::new();
    for request in ["cargo +pinned", "rustc +pinned", "cargo +pinned clippy"] {
        verifier.set_version(request, Some("1.0.0-pinned"));
    }
    let run = || outcome(&mut verifier.heli(&["+pinned", "clippy"]));
    assert_eq!(run(), Outcome::Ran);
    assert_eq!(run(), Outcome::Hit);
    for request in ["cargo +pinned", "rustc +pinned", "cargo +pinned clippy"] {
        verifier.set_version(request, Some("2.0.0-pinned"));
        assert_eq!(run(), Outcome::Ran, "{request} changed");
    }
}

/// `rustc` is the one cargo would run: `RUSTC` when it is set.
#[test]
fn rustc_is_the_one_cargo_would_run() {
    let verifier = Verifier::new();
    let rustc = verifier.tool(&verifier.fixture.path("pinned"), "rustc-pinned");
    verifier.set_version("rustc-pinned", Some("rustc 1.0.0-pinned"));
    let check = || {
        let mut command = verifier.check();
        command.env("RUSTC", &rustc);
        outcome(&mut command)
    };
    assert_eq!(check(), Outcome::Ran);
    assert_eq!(check(), Outcome::Hit);
    verifier.set_version("rustc-pinned", Some("rustc 2.0.0-pinned"));
    assert_eq!(check(), Outcome::Ran, "the pinned rustc changed");
}

/// A hit is decided before the run is admitted: it never becomes a run, so
/// it takes no run id.
#[test]
fn a_hit_is_not_a_run() {
    let verifier = Verifier::new();
    assert_eq!(outcome(&mut verifier.check()), Outcome::Ran, "r1");
    assert_eq!(outcome(&mut verifier.check()), Outcome::Hit);
    let other = || verifier.heli(&["check", "--all-targets"]);
    assert_eq!(outcome(&mut other()), Outcome::Ran);
    let hit = hit_line(&other().output().expect("run heli")).expect("a hit");
    assert!(
        hit.starts_with("cargo-heli: already passed: r2, "),
        "the second run was r2: {hit}"
    );
}

/// Within the worktree, the directory a run starts in is keyed, as an
/// argument would be: it selects the packages.
#[test]
fn the_directory_within_the_worktree_is_keyed() {
    let verifier = Verifier::new();
    let check_in = |dir: &str| {
        let mut command = verifier.check();
        command.current_dir(verifier.path(dir));
        outcome(&mut command)
    };
    assert_eq!(check_in("crates/slow-a"), Outcome::Ran);
    assert_eq!(check_in("crates/slow-a"), Outcome::Hit);
    assert_eq!(check_in("crates/slow-b"), Outcome::Ran);
}

/// A second worktree of the verifier's repository with the same content, at
/// another depth, and the search path both must use: its fake rustc lies
/// outside both worktrees, so the search path keys the same in each.
fn twin_worktree(verifier: &Verifier) -> (std::path::PathBuf, std::ffi::OsString) {
    let bin = verifier.fixture.path("shared-bin");
    verifier.tool(&bin, "rustc");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .expect("a search path");
    let other = verifier.fixture.path("nested").join("other");
    git(
        &verifier.fixture,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            other.to_str().expect("UTF-8"),
            "HEAD",
        ],
    );
    (other, path)
}

/// Waits for `child` to exit, at most a minute: a wait that is never
/// released fails the test instead of hanging it.
fn finish(child: std::process::Child) -> Output {
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(child.wait_with_output()));
    match receiver.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(output) => output.expect("wait for heli"),
        Err(_) => {
            support::kill_tree(pid);
            panic!("heli (pid {pid}) did not finish within a minute");
        }
    }
}

/// Starts `command` with its output captured.
fn start(command: &mut Command) -> std::process::Child {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start heli")
}

/// The run `status` lists with `subcommand` after its program.
fn listed<'a>(status: &'a serde_json::Value, subcommand: &str) -> Option<&'a serde_json::Value> {
    support::runs(status)
        .iter()
        .position(|run| run["argv"][1] == subcommand)
        .map(|index| &status["runs"][index])
}

/// A run waiting for its worktree's lock looks its key up again once it is
/// admitted: a pass recorded meanwhile, here by the same check in a twin
/// worktree (which does not wait), makes it a hit, and it runs nothing.
#[test]
fn a_run_recorded_while_this_one_waited_is_a_hit() {
    let verifier = Verifier::new();
    let (other, path) = twin_worktree(&verifier);
    let gate = verifier.fixture.path("gate");
    let fmt = start(verifier.heli(&["fmt"]).env("ECHO_WAIT_FOR", &gate));
    verifier
        .fixture
        .wait_for_status("fmt to hold the worktree's lock", |status| {
            listed(status, "fmt").is_some_and(|run| run["waiting_since"].is_null())
        });
    let waiting = start(verifier.check().env("PATH", &path));
    verifier
        .fixture
        .wait_for_status("the check to wait for fmt", |status| {
            listed(status, "check").is_some_and(|run| run["waiting_since"].is_u64())
        });

    let mut twin = verifier.check();
    twin.env("PATH", &path).current_dir(&other);
    assert_eq!(outcome(&mut twin), Outcome::Ran, "r3, in the twin worktree");

    std::fs::write(&gate, "").expect("open the gate");
    assert_success(&finish(fmt), "cargo heli fmt");
    let output = finish(waiting);
    let hit = hit_line(&output).unwrap_or_else(|| panic!("not a hit: {}", text(&output.stderr)));
    assert!(hit.starts_with("cargo-heli: already passed: r3, "), "{hit}");
}

/// The worktree's own path is not keyed: a pass in one worktree is a hit in
/// another with the same content, wherever it lies.
#[test]
fn a_pass_in_one_worktree_is_a_hit_in_another() {
    let verifier = Verifier::new();
    let (other, path) = twin_worktree(&verifier);
    let check_in = |dir: &std::path::Path| {
        verifier
            .check()
            .env("PATH", &path)
            .current_dir(dir)
            .output()
            .expect("run heli")
    };
    let first = check_in(&verifier.fixture.workspace());
    assert_success(&first, "the first worktree");
    assert!(hit_line(&first).is_none(), "{}", text(&first.stderr));
    let second = check_in(&other);
    let hit = hit_line(&second).unwrap_or_else(|| panic!("not a hit: {}", text(&second.stderr)));
    assert!(hit.contains("already passed: r1, "), "{hit}");
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
    // Full output, so that cargo's `Finished` line shows that it ran
    // (compact output drops it).
    let check = || {
        fixture
            .heli(["check", "-p", "slow-a"])
            .env("HELI_OUTPUT", "full")
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

/// `--no-cache` (before the subcommand) runs even when the same inputs
/// passed, and records the run when it passes.
#[test]
fn no_cache_runs_and_records_a_pass() {
    let verifier = Verifier::new();
    let forced = || verifier.heli(&["--no-cache", "check"]);
    assert_eq!(outcome(&mut forced()), Outcome::Ran);
    assert_eq!(outcome(&mut verifier.check()), Outcome::Hit, "recorded");
    assert_eq!(outcome(&mut forced()), Outcome::Ran, "forced again");
}

/// A run that fails drops the pass recorded for its key, so a flaky pass is
/// no longer reused once a forced run has failed.
#[test]
fn a_forced_run_that_fails_drops_the_pass() {
    let verifier = Verifier::new();
    let exit = verifier.fixture.path("exit-code");
    let run = |args: &[&str]| {
        let mut command = verifier.heli(args);
        command.env("ECHO_EXIT_FILE", &exit);
        command.output().expect("run heli")
    };
    assert_success(&run(&["check"]), "the first run");
    assert!(hit_line(&run(&["check"])).is_some(), "recorded");
    std::fs::write(&exit, "1").expect("write the exit code");
    let forced = run(&["--no-cache", "check"]);
    assert_eq!(forced.status.code(), Some(1), "the forced run fails");
    std::fs::remove_file(&exit).expect("remove the exit code");
    let again = run(&["check"]);
    assert_success(&again, "the next run");
    assert!(hit_line(&again).is_none(), "{}", text(&again.stderr));
}

/// The markers in the fixture's result store.
fn markers(fixture: &Fixture) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(fixture.state_dir().join("verified")) else {
        return Vec::new();
    };
    let mut markers: Vec<_> = entries
        .map(|entry| entry.expect("an entry").path())
        .collect();
    markers.sort();
    markers
}

/// Builds are never cached, and neither is `exec`, even of a check: their
/// product is not a verdict.
#[test]
fn builds_are_never_cached() {
    let fixture = Fixture::new("basic");
    init_repo(&fixture);
    for args in [
        &["build", "-p", "slow-a"][..],
        &["exec", "--", "cargo", "check", "-p", "slow-a"][..],
    ] {
        for attempt in ["first", "second"] {
            // Full output, so that cargo's `Finished` line shows that it ran
            // (compact output drops it).
            let output = fixture
                .heli(args.iter().copied())
                .env("HELI_OUTPUT", "full")
                .output()
                .expect("run heli");
            assert_success(&output, &format!("the {attempt} {args:?}"));
            assert!(
                text(&output.stderr).contains("Finished"),
                "cargo ran the {attempt} {args:?}: {}",
                text(&output.stderr)
            );
        }
    }
    assert_eq!(markers(&fixture), Vec::<std::path::PathBuf>::new());
}

/// Keying a run reads the worktree without writing the real index: `git
/// status` and the index file are as they were.
#[test]
fn the_real_index_is_untouched() {
    let verifier = Verifier::new();
    let library = verifier.path("crates/slow-a/src/lib.rs");
    let mut source = std::fs::read_to_string(&library).expect("read");
    source.push_str("// edited\n");
    std::fs::write(&library, source).expect("write");
    std::fs::write(verifier.path("untracked.txt"), "new").expect("write");
    let index = verifier.path(".git/index");
    // Without the optional locks, so `git status` itself never refreshes
    // the index.
    let status = || {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(verifier.fixture.workspace())
            .output()
            .expect("run git status");
        assert_success(&output, "git status");
        text(&output.stdout)
    };
    let before = (status(), std::fs::read(&index).expect("read the index"));
    let modified = std::fs::metadata(&index)
        .expect("stat")
        .modified()
        .expect("mtime");

    assert_eq!(outcome(&mut verifier.check()), Outcome::Ran);
    assert_eq!(outcome(&mut verifier.check()), Outcome::Hit);

    let after = (status(), std::fs::read(&index).expect("read the index"));
    assert_eq!(after, before);
    assert!(before.0.contains("?? untracked.txt"), "{}", before.0);
    assert_eq!(
        std::fs::metadata(&index)
            .expect("stat")
            .modified()
            .expect("mtime"),
        modified
    );
}

/// Sets `path`'s modification time to `age` ago.
fn age(path: &std::path::Path, age: std::time::Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now() - age))
        .expect("set the modification time");
}

/// The store keeps 256 markers: a new one beyond that drops the least
/// recently used, and a hit counts as a use.
#[test]
fn markers_beyond_the_cap_are_pruned() {
    let verifier = Verifier::new();
    let store = verifier.fixture.state_dir().join("verified");
    assert_eq!(outcome(&mut verifier.check()), Outcome::Ran);
    let [first] = &markers(&verifier.fixture)[..] else {
        panic!("one marker: {:?}", markers(&verifier.fixture));
    };
    let first = first.clone();
    let day = std::time::Duration::from_secs(86_400);
    let others: Vec<_> = (0..255u64)
        .map(|index| {
            let path = store.join(format!("{index:064x}"));
            std::fs::write(&path, "{}").expect("write a marker");
            age(&path, day + std::time::Duration::from_secs(index));
            path
        })
        .collect();
    age(&first, 2 * day);
    assert_eq!(outcome(&mut verifier.check()), Outcome::Hit, "used now");

    assert_eq!(
        outcome(&mut verifier.heli(&["check", "--all-targets"])),
        Outcome::Ran
    );
    let kept = markers(&verifier.fixture);
    assert_eq!(kept.len(), 256);
    assert!(kept.contains(&first), "the marker just used is kept");
    assert!(!kept.contains(&others[254]), "the least recently used goes");
    assert!(others[..254].iter().all(|other| kept.contains(other)));
    assert_eq!(outcome(&mut verifier.check()), Outcome::Hit);
}

/// A run that fails is not recorded: the same command runs again.
#[test]
fn a_failing_run_is_not_recorded() {
    let fixture = Fixture::new("basic");
    init_repo(&fixture);
    for attempt in ["first", "second"] {
        let output = fixture
            .heli(["check", "-p", "broken"])
            .output()
            .expect("run heli");
        assert!(!output.status.success(), "the {attempt} check fails");
        assert!(hit_line(&output).is_none(), "{attempt}");
        assert!(
            text(&output.stderr).contains("error[E0308]"),
            "cargo ran the {attempt} time: {}",
            text(&output.stderr)
        );
    }
}

/// A run whose worktree changed while it ran is not recorded, neither for
/// the tree it started on nor for the one it ended on.
#[test]
fn a_run_whose_tree_changed_is_not_recorded() {
    use std::io::BufRead;

    let verifier = Verifier::new();
    let gate = verifier.fixture.path("gate");
    let check = || {
        let mut command = verifier.check();
        command.env("ECHO_WAIT_FOR", &gate);
        command
    };
    let mut call = check();
    let mut call = call
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start heli");
    let stdout = call.stdout.take().expect("a stdout");
    let (started, running) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if line.is_ok_and(|line| line == "arg: \"check\"") {
                let _ = started.send(());
            }
        }
    });
    running
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the run started");
    let library = verifier.path("crates/slow-a/src/lib.rs");
    let original = std::fs::read(&library).expect("read");
    std::fs::write(&library, b"// edited while the run ran\n").expect("write");
    std::fs::write(&gate, "").expect("open the gate");
    let output = call.wait_with_output().expect("wait for heli");
    assert_success(&output, "the run");
    assert!(
        text(&output.stderr).contains("changed during the run"),
        "{}",
        text(&output.stderr)
    );

    assert_eq!(outcome(&mut check()), Outcome::Ran, "the tree it ended on");
    std::fs::write(&library, original).expect("restore");
    assert_eq!(
        outcome(&mut check()),
        Outcome::Ran,
        "the tree it started on"
    );
}

/// A Cargo configuration file that exists but cannot be read (here a
/// directory in its place) is keyed by its error, never as an absent file:
/// a run that cannot read it may fail, so it never shares a verdict with
/// one that has no such file.
#[test]
fn an_unreadable_cargo_configuration_is_keyed_by_its_error() {
    let verifier = Verifier::new();
    assert_each_misses(
        &verifier,
        &[Change {
            name: "an unreadable configuration in the Cargo home",
            apply: |verifier| {
                std::fs::create_dir_all(verifier.cargo_home().join("config.toml"))
                    .expect("create a directory in its place");
                verifier.check()
            },
            revert: |verifier| {
                std::fs::remove_dir(verifier.cargo_home().join("config.toml")).expect("remove");
            },
        }],
    );
}

/// `\` separates the components of a search path's entries on Windows only:
/// elsewhere `<worktree>\bin` is a directory beside the worktree, and keyed.
#[test]
fn a_backslash_separates_paths_on_windows_only() {
    let verifier = Verifier::new();
    assert_eq!(outcome(&mut verifier.check()), Outcome::Ran);
    let workspace = if cfg!(unix) {
        std::fs::canonicalize(verifier.fixture.workspace()).expect("the workspace")
    } else {
        verifier.fixture.workspace()
    };
    let mut entry = workspace.into_os_string();
    entry.push("\\bin");
    let expected = if cfg!(windows) {
        Outcome::Hit
    } else {
        Outcome::Ran
    };
    assert_eq!(
        outcome(verifier.check().env("PATH", verifier.path_with(entry))),
        expected
    );
}

/// A run whose inputs other than the tree changed while it ran (here the
/// Cargo configuration) is not recorded: after the run its whole key is
/// computed again, and a pass is recorded only under an unchanged key.
#[test]
fn a_run_whose_configuration_changed_is_not_recorded() {
    use std::io::BufRead;

    let verifier = Verifier::new();
    let gate = verifier.fixture.path("gate");
    let check = || {
        let mut command = verifier.check();
        command.env("ECHO_WAIT_FOR", &gate);
        command
    };
    let mut call = check()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start heli");
    let stdout = call.stdout.take().expect("a stdout");
    let (started, running) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if line.is_ok_and(|line| line == "arg: \"check\"") {
                let _ = started.send(());
            }
        }
    });
    running
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the run started");
    let config = verifier.cargo_home().join("config.toml");
    std::fs::write(&config, "[build]\nincremental = false\n").expect("write");
    std::fs::write(&gate, "").expect("open the gate");
    let output = call.wait_with_output().expect("wait for heli");
    assert_success(&output, "the run");
    assert!(
        text(&output.stderr).contains("changed during the run"),
        "{}",
        text(&output.stderr)
    );

    std::fs::remove_file(&config).expect("restore");
    assert_eq!(
        outcome(&mut check()),
        Outcome::Ran,
        "the configuration it started with"
    );
}