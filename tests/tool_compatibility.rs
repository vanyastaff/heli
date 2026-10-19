//! The compatibility matrix (spec #38): popular cargo tools, run through
//! heli against the `tools` fixture, keep their exit codes and output, get
//! the class their profile gives (docs/runs.md, "Tool profiles"), and,
//! where they are cached verifications, are answered from the result store
//! the second time.
//!
//! Every test here needs its real tool installed, so they are ignored by the
//! main suite. The `compatibility` workflow installs the tools and runs them
//! on Linux, macOS and Windows; those named `nightly_` need a nightly
//! toolchain (`HELI_NIGHTLY`, default `nightly`) and run in a job of their
//! own:
//!
//! ```text
//! cargo test --locked --test tool_compatibility -- --ignored --skip nightly_
//! cargo test --locked --test tool_compatibility -- --ignored nightly_
//! ```
//!
//! A missing tool fails its test with how to install it, never skips it.

mod support;

use std::process::{Command, Output, Stdio};

use support::{Fixture, assert_success, lines};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The `tools` fixture as a git repository with one commit, its lockfile
/// included, so that heli keys its runs.
struct Repo {
    fixture: Fixture,
}

impl Repo {
    fn new() -> Self {
        let fixture = Fixture::new("tools");
        let lockfile = fixture
            .cargo(["generate-lockfile", "--offline"])
            .output()
            .expect("run cargo");
        assert_success(&lockfile, "cargo generate-lockfile");
        let repo = Self { fixture };
        repo.git(&["init", "--quiet"]);
        repo.git(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.dir().join(".gitignore"), "/target/\n").expect("write .gitignore");
        repo.git(&["add", "-A"]);
        repo.git(&[
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
        ]);
        repo
    }

    fn dir(&self) -> std::path::PathBuf {
        self.fixture.workspace()
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.dir())
            .output()
            .expect("run git");
        assert_success(&output, &format!("git {}", args.join(" ")));
    }

    /// `cargo <args>` as the tool runs without heli: through rustup's
    /// proxy for a `+toolchain`, as a user's shell runs it.
    fn direct(&self, args: &[&str]) -> Output {
        let command = if args.first().is_some_and(|first| first.starts_with('+')) {
            self.fixture.command("cargo", args.iter().copied())
        } else {
            self.fixture.cargo(args.iter().copied())
        };
        as_a_user(command).output().expect("run cargo")
    }

    /// `cargo heli <args>`, with the tool's own output (`--output full`).
    fn heli(&self, args: &[&str]) -> Output {
        let args = ["--output", "full"].into_iter().chain(args.iter().copied());
        as_a_user(self.fixture.heli(args))
            .output()
            .expect("run heli")
    }

    /// Runs `cargo <args>` directly and through heli: both must exit alike,
    /// and succeed; returns heli's output.
    fn same_as_direct(&self, args: &[&str]) -> Output {
        let direct = self.direct(args);
        let through = self.heli(args);
        assert_eq!(
            direct.status.code(),
            through.status.code(),
            "cargo {} exits alike with and without heli\n--- direct\n{}{}\n--- heli\n{}{}",
            args.join(" "),
            text(&direct.stdout),
            text(&direct.stderr),
            text(&through.stdout),
            text(&through.stderr),
        );
        assert_success(&through, &format!("cargo heli {}", args.join(" ")));
        through
    }

    /// Runs `cargo heli <args>` twice: the second time it must be a hit.
    fn cached(&self, args: &[&str]) {
        let first = self.same_as_direct(args);
        assert!(hit(&first).is_none(), "the first run runs: {first:?}");
        let second = self.heli(args);
        assert!(
            hit(&second).is_some(),
            "cargo heli {} is a cached verification: the second run is a hit\n{}",
            args.join(" "),
            text(&second.stderr)
        );
    }
}

/// `command` as a user's shell would start it: without what cargo sets for
/// this test binary (`CARGO_PKG_*`), which some tools read as their own
/// (cargo-machete then takes its subcommand's name for a path), and with no
/// stdin.
fn as_a_user(mut command: Command) -> Command {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("CARGO_PKG_") {
            command.env_remove(name);
        }
    }
    command.stdin(Stdio::null());
    command
}

/// The line that reports a result-store hit, if `output` has one.
fn hit(output: &Output) -> Option<String> {
    lines(&output.stderr)
        .into_iter()
        .find(|line| line.starts_with("cargo-heli: already passed: "))
}

/// Fails, with how to install it, unless `cargo <tool>` is installed: cargo
/// lists it among its commands (not every tool's `--version` exits 0).
fn require(tool: &str) {
    let listed = Command::new("cargo")
        .arg("--list")
        // Plain names: CI sets CARGO_TERM_COLOR=always.
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .output()
        .expect("run cargo --list");
    let installed = lines(&listed.stdout)
        .iter()
        .any(|line| line.split_whitespace().next() == Some(tool));
    assert!(
        installed,
        "cargo-{tool} is not installed; install it (for example \
         `cargo binstall cargo-{tool}` or `cargo install cargo-{tool} --locked`) \
         to run the compatibility matrix"
    );
}

#[test]
#[ignore = "needs cargo-hack; run by the compatibility workflow"]
fn hack_checks_are_cached_verifications() {
    require("hack");
    Repo::new().cached(&["hack", "check", "--each-feature", "--workspace"]);
}

#[test]
#[ignore = "needs cargo-machete; run by the compatibility workflow"]
fn machete_is_a_cached_verification() {
    require("machete");
    Repo::new().cached(&["machete"]);
}

#[test]
#[ignore = "needs cargo-shear; run by the compatibility workflow"]
fn shear_is_a_cached_verification() {
    require("shear");
    Repo::new().cached(&["shear"]);
}

#[test]
#[ignore = "needs cargo-deny; run by the compatibility workflow"]
fn deny_checks_without_advisories_are_cached() {
    require("deny");
    Repo::new().cached(&["deny", "check", "bans", "licenses", "sources"]);
}

/// cargo-audit, with a database of the test's own: heli fetches it before
/// the key, so the second run is a hit.
#[test]
#[ignore = "needs cargo-audit and the network; run by the compatibility workflow"]
fn audit_is_cached_against_the_database_heli_fetched() {
    require("audit");
    let repo = Repo::new();
    let db = repo.fixture.path("advisory-db");
    let db = db.to_str().expect("a Unicode path");
    repo.cached(&["audit", "--db", db]);
}

/// cargo-llvm-cov keeps its exit code, and its own target directory is the
/// one heli counts as a tool cache.
#[test]
#[ignore = "needs cargo-llvm-cov and llvm-tools; run by the compatibility workflow"]
fn llvm_cov_keeps_its_result_and_its_target_directory() {
    require("llvm-cov");
    let repo = Repo::new();
    repo.same_as_direct(&["llvm-cov", "--workspace", "--summary-only"]);
    assert!(
        repo.fixture.target_dir().join("llvm-cov-target").is_dir(),
        "llvm-cov builds in target/llvm-cov-target"
    );
}

/// cargo-mutants lists the same mutants through heli.
#[test]
#[ignore = "needs cargo-mutants; run by the compatibility workflow"]
fn mutants_lists_the_same_mutants() {
    require("mutants");
    let repo = Repo::new();
    let direct = repo.direct(&["mutants", "--list"]);
    let through = repo.same_as_direct(&["mutants", "--list"]);
    assert_eq!(text(&through.stdout), text(&direct.stdout));
}

/// `insta review`, interactive, runs in the client: with nothing to review
/// it ends as it does without heli.
#[test]
#[ignore = "needs cargo-insta; run by the compatibility workflow"]
fn insta_review_runs_directly() {
    require("insta");
    let output = Repo::new().same_as_direct(&["insta", "review"]);
    assert!(
        !text(&output.stderr).contains("running the command directly"),
        "no fallback warning: {}",
        text(&output.stderr)
    );
}

/// The nightly toolchain the `nightly_` tests use: `HELI_NIGHTLY`, else
/// `nightly`. Fails, with how to install it, unless its cargo answers.
fn nightly() -> String {
    let toolchain = std::env::var("HELI_NIGHTLY")
        .ok()
        .filter(|toolchain| !toolchain.is_empty())
        .unwrap_or_else(|| "nightly".to_owned());
    let answers = Command::new("cargo")
        .arg(format!("+{toolchain}"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        answers,
        "toolchain {toolchain} has no cargo; install it \
         (`rustup toolchain install {toolchain}`) or set HELI_NIGHTLY"
    );
    toolchain
}

/// cargo-hakari: `generate` rewrites the workspace-hack crate (mutate),
/// `verify` is a cached verification.
#[test]
#[ignore = "needs cargo-hakari; run by the compatibility workflow"]
fn hakari_generate_mutates_and_verify_is_cached() {
    require("hakari");
    let repo = Repo::new();
    let init = repo.direct(&["hakari", "init", "workspace-hack", "--yes"]);
    assert_success(&init, "cargo hakari init");
    repo.same_as_direct(&["hakari", "generate"]);
    repo.cached(&["hakari", "verify"]);
}

#[test]
#[ignore = "needs cargo-udeps and a nightly toolchain; run by the compatibility workflow"]
fn nightly_udeps_is_a_cached_verification() {
    require("udeps");
    let toolchain = format!("+{}", nightly());
    Repo::new().cached(&[&toolchain, "udeps", "--workspace"]);
}

#[test]
#[ignore = "needs cargo-public-api and a nightly toolchain; run by the compatibility workflow"]
fn nightly_public_api_is_a_cached_verification() {
    require("public-api");
    let toolchain = format!("+{}", nightly());
    let repo = Repo::new();
    let direct = repo.direct(&[&toolchain, "public-api", "-p", "util"]);
    assert!(
        text(&direct.stdout).contains("pub fn util::base() -> u32"),
        "{}",
        text(&direct.stdout)
    );
    repo.cached(&[&toolchain, "public-api", "-p", "util"]);
}

/// `miri test`, a cached verification keyed by the nightly toolchain.
#[test]
#[ignore = "needs Miri on a nightly toolchain; run by the compatibility workflow"]
fn nightly_miri_test_is_a_cached_verification() {
    let toolchain = format!("+{}", nightly());
    let repo = Repo::new();
    let setup = repo.direct(&[&toolchain, "miri", "setup"]);
    assert_success(&setup, "cargo miri setup");
    repo.cached(&[&toolchain, "miri", "test", "-p", "util"]);
}

/// `cargo remove` and `cargo add`, mutate runs, edit the manifest as they do
/// without heli.
#[test]
fn add_and_remove_edit_the_manifest() {
    let repo = Repo::new();
    let manifest = repo.dir().join("app").join("Cargo.toml");
    let original = std::fs::read_to_string(&manifest).expect("read the manifest");
    let remove = repo.heli(&["remove", "-p", "app", "util", "--offline"]);
    assert_success(&remove, "cargo heli remove");
    let removed = std::fs::read_to_string(&manifest).expect("read the manifest");
    assert!(!removed.contains("util"), "{removed}");
    // `--path` is relative to the working directory, the workspace's root.
    let add = repo.heli(&["add", "-p", "app", "--path", "util", "--offline"]);
    assert_success(&add, "cargo heli add");
    let added = std::fs::read_to_string(&manifest).expect("read the manifest");
    assert!(added.contains("util"), "{added}\n--- before\n{original}");
}

/// `insta review` on a real terminal: interactive, it runs in the client
/// and ends as it does without heli (#64).
#[test]
#[ignore = "needs cargo-insta; run by the compatibility workflow"]
fn insta_review_runs_on_a_terminal() {
    require("insta");
    let repo = Repo::new();
    let mut command = repo.fixture.heli(["insta", "review"]);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("CARGO_PKG_") {
            command.env_remove(name);
        }
    }
    let shown = support::terminal::run(&mut command);
    assert_eq!(shown.code, Some(0), "{}", shown.text);
    assert!(
        !shown.text.contains("running the command directly"),
        "no fallback warning: {}",
        shown.text
    );
}

/// The host pool bounds a real tool's cargos (#64): two `cargo heli hack
/// check` runs at once, with a pool of two slots, never run more than the
/// pool's two jobs plus one of each cargo's own at once. Their argv differ,
/// so heli runs both rather than sharing one; the slow crates are compiled
/// beforehand, so what is measured is how the pool schedules their build
/// scripts.
#[test]
#[ignore = "needs cargo-hack; run by the compatibility workflow"]
fn the_pool_bounds_cargo_hack() {
    require("hack");
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let runs = [
        ("a", ["-p", "slow-a", "-p", "slow-b"]),
        ("b", ["-p", "slow-b", "-p", "slow-c"]),
    ];
    let logs: Vec<support::ConcurrencyLog> = runs
        .iter()
        .map(|(name, _)| fixture.concurrency_log_named(name))
        .collect();
    for ((name, _), log) in runs.iter().zip(&logs) {
        log.warm_slow_crates(&fixture, &format!("target-{name}"));
    }
    let children: Vec<std::process::Child> = runs
        .iter()
        .zip(&logs)
        .map(|((name, packages), log)| {
            let mut command = fixture.heli(["hack", "check"]);
            command
                .args(packages)
                .env("CARGO_TARGET_DIR", fixture.path(&format!("target-{name}")))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (name, _) in std::env::vars_os() {
                if name.to_string_lossy().starts_with("CARGO_PKG_") {
                    command.env_remove(name);
                }
            }
            log.attach(&mut command, 3000);
            command.spawn().expect("start heli")
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().expect("wait for heli");
        assert_success(&output, "cargo heli hack check");
    }
    let per_run: Vec<Vec<support::Interval>> = logs.iter().map(|log| log.intervals()).collect();
    assert!(
        per_run.iter().all(|run| !run.is_empty()),
        "both runs' build scripts ran"
    );
    // They ran at once, or the bound would say nothing.
    for (run, other) in [(0, 1), (1, 0)] {
        let first_start = per_run[run].iter().map(|script| script.start).min();
        let last_end = per_run[other].iter().map(|script| script.end).max();
        assert!(
            first_start < last_end,
            "run {run} waited for run {other}: {per_run:?}"
        );
    }
    let most = support::max_overlap(&per_run.concat());
    assert!(
        most <= 2 + 2,
        "{most} build scripts ran at once: {per_run:?}"
    );
}
