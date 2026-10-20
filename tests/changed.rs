//! `--changed`, `--affected` and `cargo heli changed` (docs/runs.md): the
//! workspace members whose files changed since a base, and with
//! `--affected` the members depending on them, become the run's `-p` list.
//!
//! The fixture is three crates, leaf ← mid ← top (`top` dev-depends on
//! `mid`), and `docs/`, which no package holds. Each test makes it a git
//! repository whose one commit is the branch `base`.

mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use support::{Fixture, assert_success, lines};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `git <args>` in the fixture's workspace; panics unless it succeeds.
fn git(fixture: &Fixture, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(fixture.workspace())
        .output()
        .expect("run git");
    assert_success(&output, &format!("git {}", args.join(" ")));
    text(&output.stdout)
}

/// Commits everything in the fixture's workspace, with message `message`.
fn commit(fixture: &Fixture, message: &str) {
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
            message,
        ],
    );
}

/// The three-crate fixture as a git repository with one commit, `Cargo.lock`
/// included, which the branch `base` also names; `ignored/` is ignored.
fn repository() -> Fixture {
    let fixture = Fixture::new("changed");
    std::fs::write(fixture.workspace().join(".gitignore"), "/ignored/\n")
        .expect("write .gitignore");
    let lockfile = fixture
        .cargo(["generate-lockfile", "--offline"])
        .output()
        .expect("run cargo");
    assert_success(&lockfile, "cargo generate-lockfile");
    git(&fixture, &["init", "--quiet"]);
    git(&fixture, &["config", "core.autocrlf", "false"]);
    commit(&fixture, "fixture");
    git(&fixture, &["branch", "base"]);
    fixture
}

/// Appends a comment line to `file` in the fixture's workspace.
fn touch(fixture: &Fixture, file: &str) {
    let path = fixture.workspace().join(file);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create its directory");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let comment = if file.ends_with(".rs") { "//" } else { "#" };
    text.push_str(&format!("\n{comment} changed\n"));
    std::fs::write(&path, text).expect("change the file");
}

/// `cargo heli changed <args> --json`: its document.
fn selection(fixture: &Fixture, args: &[&str]) -> Value {
    let output = fixture
        .heli(["changed"].iter().chain(args).chain(&["--json"]))
        .output()
        .expect("run cargo-heli");
    assert_success(&output, "cargo heli changed");
    serde_json::from_slice(&output.stdout).expect("a JSON document")
}

/// The cargo arguments of a selection document.
fn args(document: &Value) -> Vec<&str> {
    document["args"]
        .as_array()
        .expect("args")
        .iter()
        .map(|arg| arg.as_str().expect("a string"))
        .collect()
}

/// The line heli printed about its selection, if any.
fn selection_line(output: &Output) -> Option<String> {
    lines(&output.stderr).into_iter().find(|line| {
        line.starts_with("cargo-heli: --changed") || line.starts_with("cargo-heli: --affected")
    })
}

fn assert_refused(output: &Output, says: &str) {
    assert_eq!(output.status.code(), Some(2), "refused: {output:?}");
    assert!(
        text(&output.stderr).contains(says),
        "the refusal says {says:?}: {output:?}"
    );
}

/// A change to `leaf` selects `-p leaf`; `--affected` adds `mid`, which
/// depends on it, and `top`, which dev-depends on `mid`. Committed,
/// staged, unstaged and untracked changes all count; ignored files and
/// files outside any member do not.
#[test]
fn a_change_to_leaf_selects_it_and_its_dependents() {
    let fixture = repository();
    touch(&fixture, "leaf/src/lib.rs");
    commit(&fixture, "change leaf");
    std::fs::write(fixture.workspace().join("leaf/src/new.rs"), "").expect("untracked file");
    touch(&fixture, "ignored/top/src/lib.rs");
    touch(&fixture, "docs/notes.md");

    let changed = selection(&fixture, &["--base", "base"]);
    assert_eq!(args(&changed), ["-p", "leaf"], "{changed}");
    assert_eq!(
        changed["packages"][0]["files"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(changed["outside"], serde_json::json!(["docs/notes.md"]));

    let affected = selection(&fixture, &["--affected", "--base", "base"]);
    assert_eq!(args(&affected), ["-p", "leaf", "-p", "mid", "-p", "top"]);
    assert_eq!(affected["packages"][2]["name"], "top");
    assert_eq!(affected["packages"][2]["depends_on"], "mid");
}

/// The root manifest, `Cargo.lock` and `.cargo/` select the whole
/// workspace.
#[test]
fn a_root_manifest_change_selects_the_workspace() {
    let fixture = repository();
    touch(&fixture, "Cargo.toml");
    let document = selection(&fixture, &["--base", "base"]);
    assert_eq!(args(&document), ["--workspace"], "{document}");
    assert_eq!(document["workspace"], serde_json::json!(["Cargo.toml"]));
}

/// A run with `--changed` gets the `-p` list and says so; `cargo check`
/// then checks `leaf` alone.
#[test]
fn a_run_with_changed_checks_only_the_changed_package() {
    let fixture = repository();
    touch(&fixture, "leaf/src/lib.rs");
    let output = fixture
        .heli(["--output", "full", "--changed=base", "check"])
        .output()
        .expect("run cargo-heli");
    assert_success(&output, "cargo heli --changed=base check");
    let line = selection_line(&output).expect("the selection line");
    assert!(
        line.starts_with("cargo-heli: --changed since base (") && line.ends_with("): -p leaf"),
        "{line}"
    );
    let stderr = text(&output.stderr);
    assert!(stderr.contains("Checking leaf"), "{stderr}");
    assert!(!stderr.contains("Checking mid"), "{stderr}");

    let affected = fixture
        .heli(["--output", "full", "--affected=base", "check"])
        .output()
        .expect("run cargo-heli");
    assert_success(&affected, "cargo heli --affected=base check");
    let line = selection_line(&affected).expect("the selection line");
    assert!(
        line.ends_with("): -p leaf -p mid -p top (+2 dependents)"),
        "{line}"
    );
}

/// nextest's `-E` is a test filter, not a package chooser: beside a
/// selection it keeps the selected packages' tests that match, as it does
/// beside `-p`; `package(…)` in it narrows the selection, and adds nothing.
#[test]
fn a_nextest_filterset_filters_within_the_selection() {
    support::require_nextest();
    let fixture = repository();
    touch(&fixture, "leaf/src/lib.rs");
    let run = |args: &[&str]| {
        let output = fixture
            .heli(["--output", "compact"].iter().chain(args))
            .output()
            .expect("run cargo-heli");
        assert_success(&output, &format!("cargo heli {}", args.join(" ")));
        output
    };

    // `leaf` alone, and of its two tests only `one`.
    let changed = run(&["--changed=base", "nextest", "run", "-E", "test(one)"]);
    let line = selection_line(&changed).expect("the selection line");
    assert!(line.ends_with("): -p leaf"), "{line}");
    let stderr = text(&changed.stderr);
    assert!(
        stderr.contains("1 test run: 1 passed, 1 skipped"),
        "{stderr}"
    );

    // `leaf`, `mid` and `top`, and of their tests only `top`'s: nextest
    // skips the two other test binaries whole.
    let affected = run(&["--affected=base", "nextest", "run", "-E", "package(top)"]);
    let line = selection_line(&affected).expect("the selection line");
    assert!(
        line.ends_with("): -p leaf -p mid -p top (+2 dependents)"),
        "{line}"
    );
    let stderr = text(&affected.stderr);
    assert!(stderr.contains("1 test run: 1 passed"), "{stderr}");
    assert!(stderr.contains("2 binaries skipped"), "{stderr}");
}

/// An unchanged tree runs nothing: heli says so and exits 0.
#[test]
fn an_unchanged_tree_runs_nothing() {
    let fixture = repository();
    let output = fixture
        .heli(["--changed=base", "check"])
        .output()
        .expect("run cargo-heli");
    assert_success(&output, "cargo heli --changed=base check");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("cargo-heli: nothing changed since base ("),
        "{stderr}"
    );
    assert!(!stderr.contains("Checking"), "nothing ran: {stderr}");
    assert!(
        !fixture.target_dir().join("debug").exists(),
        "nothing was built"
    );
}

/// Without `=REF`, the base is the branch's upstream; without one (and no
/// `origin/HEAD`), heli refuses and says to give one.
#[test]
fn the_default_base_is_the_upstream() {
    let fixture = repository();
    let refused = fixture
        .heli(["--changed", "check"])
        .output()
        .expect("run cargo-heli");
    assert_refused(&refused, "--changed=REF");

    git(&fixture, &["branch", "--set-upstream-to=base"]);
    touch(&fixture, "mid/src/lib.rs");
    let document = selection(&fixture, &[]);
    assert_eq!(document["base"]["reference"], "base");
    assert_eq!(args(&document), ["-p", "mid"]);
}

/// heli refuses a selection with arguments that choose packages, for a
/// subcommand that takes no `-p`, and outside a cargo subcommand.
#[test]
fn a_selection_is_refused_where_it_cannot_apply() {
    let fixture = repository();
    let run = |args: &[&str]| fixture.heli(args).output().expect("run cargo-heli");
    for chooser in [
        &["--changed=base", "check", "-p", "leaf"][..],
        &["--affected=base", "build", "--package=leaf"],
        &["--changed=base", "clippy", "--workspace"],
        &["--changed=base", "test", "--all"],
        &["--changed=base", "check", "--exclude", "top"],
    ] {
        assert_refused(&run(chooser), "cannot be combined");
    }
    assert_refused(&run(&["--changed=base", "fmt"]), "which take -p");
    assert_refused(
        &run(&["--changed=base", "nextest", "archive"]),
        "which take -p",
    );
    assert_refused(
        &run(&["--affected", "exec", "--", "git", "status"]),
        "only before a cargo subcommand",
    );
    assert_refused(
        &run(&["--changed", "--affected", "check"]),
        "cannot be used with",
    );
    assert!(!Path::new(&fixture.target_dir()).join("debug").exists());
}
