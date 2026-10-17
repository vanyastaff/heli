//! A run is keyed by its local sources (`DESIGN.md` §7) even when its Cargo
//! home lacks packages it does not build: optional dependencies no feature
//! enables, and dependencies of other platforms. A private Cargo home (`[cargo]
//! home = "isolated"`) holds only what its builds downloaded, so a resolution
//! of every feature for every platform cannot run offline there (#66).
//!
//! The registry here is a local one whose index names `optional-dep` and
//! `other-platform-dep` but holds neither package: as in a warm private home,
//! cargo resolves the lockfile, and builds, without them.

mod support;

use std::path::Path;
use std::process::{Command, Output};

use support::{Fixture, assert_success, lines};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create a directory");
    std::fs::write(path, text).expect("write a file");
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert_success(&output, &format!("git {}", args.join(" ")));
}

/// An index entry of a local registry for `name` 1.0.0, without its package.
fn index_entry(registry: &Path, name: &str) {
    let dir = registry.join("index").join(match name.len() {
        1 => "1".to_owned(),
        2 => "2".to_owned(),
        3 => format!("3/{}", &name[..1]),
        _ => format!("{}/{}", &name[..2], &name[2..4]),
    });
    write(
        &dir.join(name),
        &format!(
            "{{\"name\":\"{name}\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{}\",\"features\":{{}},\"yanked\":false}}\n",
            "0".repeat(64)
        ),
    );
}

/// A git repository holding a package with an optional and an
/// other-platform registry dependency that the registry lacks, a path
/// dependency and a `[patch]` to another local crate.
fn workspace(fixture: &Fixture) -> std::path::PathBuf {
    let registry = fixture.path("registry");
    index_entry(&registry, "optional-dep");
    index_entry(&registry, "other-platform-dep");
    index_entry(&registry, "patched-dep");
    let root = fixture.path("project");
    write(
        &root.join("Cargo.toml"),
        "[package]\nname = \"project\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\noptional-dep = { version = \"1\", optional = true }\n\
         local = { path = \"local\" }\npatched-dep = \"1\"\n\n\
         [target.'cfg(target_os = \"none\")'.dependencies]\nother-platform-dep = \"1\"\n\n\
         [patch.crates-io]\npatched-dep = { path = \"patched\" }\n",
    );
    write(
        &root.join("src/lib.rs"),
        "pub fn value() -> u32 { local::value() + patched_dep::value() }\n",
    );
    write(
        &root.join("local/Cargo.toml"),
        "[package]\nname = \"local\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        &root.join("local/src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    );
    write(
        &root.join("patched/Cargo.toml"),
        "[package]\nname = \"patched-dep\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
    );
    write(
        &root.join("patched/src/lib.rs"),
        "pub fn value() -> u32 { 2 }\n",
    );
    write(
        &root.join(".cargo/config.toml"),
        &format!(
            "[source.crates-io]\nreplace-with = \"local-registry\"\n\n\
             [source.local-registry]\nlocal-registry = '{}'\n",
            registry.display()
        ),
    );
    write(&root.join(".gitignore"), "/target/\n");
    let lockfile = fixture
        .cargo(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .output()
        .expect("run cargo");
    assert_success(&lockfile, "cargo generate-lockfile");
    git(&root, &["init", "--quiet"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    git(&root, &["add", "-A"]);
    git(
        &root,
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
            "project",
        ],
    );
    root
}

fn check(fixture: &Fixture, root: &Path) -> Output {
    let output = fixture
        .heli(["check", "--offline"])
        .current_dir(root)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli check");
    output
}

fn hit(output: &Output) -> bool {
    lines(&output.stderr)
        .iter()
        .any(|line| line.starts_with("cargo-heli: already passed: "))
}

/// The second identical check is a hit: the run was keyed, though cargo
/// cannot resolve every feature for every platform offline.
#[test]
fn a_run_is_keyed_without_its_unused_registry_packages() {
    let fixture = Fixture::new("basic");
    let root = workspace(&fixture);
    let first = check(&fixture, &root);
    assert!(
        !lines(&first.stderr)
            .iter()
            .any(|line| line.contains("cannot key this run")),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(hit(&check(&fixture, &root)), "the second check is a hit");
}

/// The fallback inventory still covers every local source: a change to the
/// path dependency, or to the `[patch]`ed crate, misses.
#[test]
fn a_change_to_a_path_or_patched_dependency_misses() {
    let fixture = Fixture::new("basic");
    let root = workspace(&fixture);
    check(&fixture, &root);
    assert!(hit(&check(&fixture, &root)));
    for file in ["local/src/lib.rs", "patched/src/lib.rs"] {
        let path = root.join(file);
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, text.replace("{ ", "{ 0 + ")).expect("change it");
        assert!(!hit(&check(&fixture, &root)), "{file} changed");
        assert!(hit(&check(&fixture, &root)), "{file} unchanged since");
    }
}
