//! The compatibility matrix (spec #38): popular cargo tools, run through
//! heli against the `tools` fixture, keep their exit codes and output, get
//! the class their profile gives (`DESIGN.md` §5, "Tool profiles"), and,
//! where they are cached verifications, are answered from the result store
//! the second time.
//!
//! Every test here needs its real tool installed, so they are ignored by the
//! main suite. The `compatibility` workflow installs the tools and runs them
//! on Linux, macOS and Windows:
//!
//! ```text
//! cargo test --locked --test tool_compatibility -- --ignored
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

    /// `cargo <args>` as the tool runs without heli.
    fn direct(&self, args: &[&str]) -> Output {
        as_a_user(self.fixture.cargo(args.iter().copied()))
            .output()
            .expect("run cargo")
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
