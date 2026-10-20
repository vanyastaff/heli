//! Isolated Cargo homes in the build store's accounting and eviction
//! (docs/build-store.md#worktree-cargo-homes, #120), through the public
//! CLI: `cargo heli gc` lists each home with its worktree and `status`
//! counts it; the ladder retires the home of a removed worktree once it is
//! unclaimed for `orphan_hours`, the home of no known worktree once idle
//! that long, and the home of a live worktree once idle for `dir_hours`;
//! and a home a cargo builds with is busy, whatever its age.
//!
//! Every home is in the fixture's state directory, `<state>/h/`, and every
//! build runs without the harness's `CARGO_TARGET_DIR`, so the worktree's
//! own `target/` gets the artifacts and goes with it.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use support::{Fixture, assert_success};

const HOUR: u64 = 3600;

/// `[store]` lines that make the ladder plan: over a budget of nothing,
/// everything evictable is a victim.
const PLAN: &str = "budget_mb = 0\n";

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Now, in whole seconds since 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs()
}

/// Polls `check` until it returns something, and returns that; panics after
/// 60 s, naming `what` it waited for.
fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `git <args>` in `dir`, with no hooks and no signing; panics unless it
/// succeeds.
fn git(dir: &Path, args: &[&str]) {
    let hooks = dir.join(".no-hooks");
    std::fs::create_dir_all(&hooks).expect("create the hooks directory");
    let output = Command::new("git")
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()))
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert_success(&output, &format!("git {}", args.join(" ")));
}

/// A worktree `name` in the fixture: a git repository holding a library
/// crate with no dependencies, so that it checks offline. With `gated`, its
/// build script says it started in the file `HOME_STARTED` names and then
/// waits for the file `HOME_GATE` names, so a build can be held open.
fn worktree(fixture: &Fixture, name: &str, gated: bool) -> PathBuf {
    let root = fixture.path(name);
    std::fs::create_dir_all(root.join("src")).expect("create the worktree");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"home-canary\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(
        root.join("src").join("lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("write lib.rs");
    if gated {
        std::fs::write(
            root.join("build.rs"),
            r#"
use std::{env, fs, path::Path, thread, time::{Duration, Instant}};
fn main() {
    println!("cargo:rerun-if-env-changed=HOME_GATE");
    if let Some(started) = env::var_os("HOME_STARTED") {
        fs::write(started, "").unwrap();
    }
    if let Some(gate) = env::var_os("HOME_GATE") {
        let start = Instant::now();
        while !Path::new(&gate).is_file() {
            assert!(start.elapsed() < Duration::from_secs(60), "bounded gate");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
"#,
        )
        .expect("write build.rs");
    }
    git(&root, &["init", "--quiet"]);
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=heli",
            "-c",
            "user.email=heli@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "canary",
        ],
    );
    root
}

/// Configures the fixture's store, with `extra` lines after it in `[store]`.
fn configure(fixture: &Fixture, extra: &str) {
    fixture.configure(&format!(
        "[store]\npaths = ['{}']\n{extra}",
        fixture.path("store").display()
    ));
}

/// `cargo heli --cargo-home isolated check --offline` in `root`, with the
/// original Cargo home `<fixture>/original-home` (empty: no configuration
/// and no credentials to carry over), building into `root`'s own `target/`.
fn isolated(fixture: &Fixture, root: &Path) -> Command {
    let mut command = fixture.heli([
        "--cache",
        "off",
        "--cargo-home",
        "isolated",
        "check",
        "--offline",
    ]);
    command
        .current_dir(root)
        .env("CARGO_HOME", fixture.path("original-home"))
        .env_remove("CARGO_TARGET_DIR");
    command
}

/// Runs an isolated check in `root`, and returns the path of the home it
/// selected, as `gc` lists it.
fn check_in(fixture: &Fixture, root: &Path) -> PathBuf {
    let output = isolated(fixture, root).output().expect("run heli check");
    assert_success(&output, &format!("an isolated check in {}", root.display()));
    let listed = gc(fixture, &["gc", "--dry-run"]);
    let worktree = std::fs::canonicalize(root).expect("the worktree exists");
    let home = homes(&listed)
        .into_iter()
        .find(|home| {
            home["workspace"]
                .as_str()
                .and_then(|path| std::fs::canonicalize(path).ok())
                == Some(worktree.clone())
        })
        .unwrap_or_else(|| panic!("no home of {}: {listed}", root.display()));
    assert_eq!(home["state"], "live", "{home}");
    assert!(home["store"].is_null(), "{home}");
    assert!(
        home["repository"].is_null(),
        "a home is a worktree's: {home}"
    );
    assert!(home["last_run"].is_u64(), "{home}");
    assert!(home["size"].as_u64() > Some(0), "{home}");
    let path = PathBuf::from(home["path"].as_str().expect("a path"));
    assert!(
        path.starts_with(fixture.state_dir().join("h")),
        "a home is in the state directory: {}",
        path.display()
    );
    path
}

/// `cargo heli gc <args> --json`, and the document it prints.
fn gc(fixture: &Fixture, args: &[&str]) -> Value {
    let output = fixture
        .heli(args.iter().copied().chain(["--json"]))
        .output()
        .expect("run heli gc");
    assert_success(&output, &format!("cargo heli {}", args.join(" ")));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("not JSON ({error}): {}", text(&output.stdout));
    })
}

/// `cargo heli gc --dry-run`, as text.
fn gc_text(fixture: &Fixture) -> String {
    let output = fixture
        .heli(["gc", "--dry-run"])
        .output()
        .expect("run heli gc");
    assert_success(&output, "cargo heli gc --dry-run");
    text(&output.stdout)
}

/// The `home` entries of a gc document.
fn homes(document: &Value) -> Vec<Value> {
    document["dirs"]
        .as_array()
        .unwrap_or_else(|| panic!("no dirs: {document}"))
        .iter()
        .filter(|entry| entry["kind"] == "home")
        .cloned()
        .collect()
}

/// The entry of a gc document at `path`, if it lists one.
fn listed(document: &Value, path: &Path) -> Option<Value> {
    document["dirs"]
        .as_array()
        .unwrap_or_else(|| panic!("no dirs: {document}"))
        .iter()
        .find(|entry| entry["path"].as_str().map(Path::new) == Some(path))
        .cloned()
}

/// The victim of a gc document at `path`, as (tier, reason), if any.
fn victim(document: &Value, path: &Path) -> Option<(u64, String)> {
    document["victims"]
        .as_array()
        .unwrap_or_else(|| panic!("no victims: {document}"))
        .iter()
        .find(|victim| victim["path"].as_str().map(Path::new) == Some(path))
        .map(|victim| {
            (
                victim["tier"].as_u64().expect("a tier"),
                victim["reason"].as_str().expect("a reason").to_owned(),
            )
        })
}

/// Sets the clock of the fixture's heli to `at`, seconds since 1970.
fn set_clock(fixture: &Fixture, at: u64) {
    fixture.set_clock(at * 1000);
}

/// `cargo heli gc` until `path` is gone, and its trash is empty: a pass
/// right after a run may find the home busy (the daemon holds the run's
/// lease a moment after its client exits), and Windows may defer a
/// deletion. Returns the document of the pass that evicted it.
fn evict(fixture: &Fixture, path: &Path) -> Value {
    let trash = fixture.state_dir().join("h").join(".heli-trash");
    let mut evicted = None;
    wait_for("the home to be evicted and its trash emptied", || {
        let document = gc(fixture, &["gc"]);
        if victim(&document, path).is_some() {
            evicted = Some(document);
        }
        let pending = std::fs::read_dir(&trash).map_or(0, |entries| entries.count());
        (!path.exists() && pending == 0).then_some(())
    });
    evicted.unwrap_or_else(|| panic!("{} went without a pass evicting it", path.display()))
}

#[test]
fn the_home_of_a_removed_worktree_is_listed_then_retired() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let root = worktree(&fixture, "wt", false);
    let home = check_in(&fixture, &root);
    assert!(
        gc_text(&fixture).contains("home    live"),
        "{}",
        gc_text(&fixture)
    );
    let status = fixture.wait_for_status("the home counted", |status| {
        status["disk"]["homes"].as_array().is_some_and(|homes| {
            homes
                .iter()
                .any(|entry| entry["path"].as_str().map(Path::new) == Some(home.as_path()))
        })
    });
    let counted = &status["disk"]["homes"];
    assert!(
        counted
            .as_array()
            .expect("homes")
            .iter()
            .all(|entry| entry["used"].as_u64() > Some(0)),
        "{counted}"
    );

    // Live and idle: kept for a day, a victim after dir_hours.
    let clock = now() + 25 * HOUR;
    set_clock(&fixture, clock);
    configure(&fixture, PLAN);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(listed(&planned, &home).expect("listed")["state"], "live");
    assert_eq!(victim(&planned, &home), None, "{planned}");
    let clock = clock + 150 * HOUR;
    set_clock(&fixture, clock);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    let (tier, reason) = victim(&planned, &home).unwrap_or_else(|| panic!("{planned}"));
    assert_eq!(tier, 5, "{planned}");
    assert!(reason.starts_with("Cargo home idle for"), "{reason}");

    // Its worktree removed: an orphan, unclaimed from the first pass that
    // notes it, a victim after orphan_hours.
    std::fs::remove_dir_all(&root).expect("remove the worktree");
    configure(&fixture, "");
    let noted = wait_for("the home to be an orphan", || {
        let document = gc(&fixture, &["gc"]);
        (listed(&document, &home)?["state"] == "orphan").then_some(document)
    });
    assert_eq!(victim(&noted, &home), None, "{noted}");
    assert!(
        gc_text(&fixture).contains("home    orphan"),
        "{}",
        gc_text(&fixture)
    );
    configure(&fixture, PLAN);
    set_clock(&fixture, clock + HOUR);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(
        victim(&planned, &home),
        None,
        "unclaimed for an hour only: {planned}"
    );
    set_clock(&fixture, clock + 25 * HOUR);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    let (tier, reason) = victim(&planned, &home).unwrap_or_else(|| panic!("{planned}"));
    assert_eq!(tier, 2, "{planned}");
    assert!(
        reason.starts_with("Cargo home of a removed worktree, unclaimed for"),
        "{reason}"
    );
    let evicted = evict(&fixture, &home);
    assert_eq!(victim(&evicted, &home).map(|(tier, _)| tier), Some(2));
    assert!(listed(&gc(&fixture, &["gc", "--dry-run"]), &home).is_none());
}

#[test]
fn a_home_of_no_known_worktree_is_retired_once_idle() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let root = worktree(&fixture, "wt", false);
    let home = check_in(&fixture, &root);
    // As an older heli published it: nothing says whose it is.
    std::fs::remove_dir_all(home.join(".heli")).expect("forget the worktree");
    let listed_now = gc(&fixture, &["gc", "--dry-run"]);
    let entry = listed(&listed_now, &home).expect("listed");
    assert_eq!(entry["state"], "unknown", "{entry}");
    assert!(entry["workspace"].is_null(), "{entry}");
    configure(&fixture, PLAN);
    set_clock(&fixture, now() + HOUR);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(victim(&planned, &home), None, "idle for an hour: {planned}");
    set_clock(&fixture, now() + 25 * HOUR);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    let (tier, reason) = victim(&planned, &home).unwrap_or_else(|| panic!("{planned}"));
    assert_eq!(tier, 2, "{planned}");
    assert!(
        reason.starts_with("Cargo home of no known worktree, idle for"),
        "{reason}"
    );
    evict(&fixture, &home);
    assert!(
        root.join("Cargo.toml").is_file(),
        "the worktree is untouched"
    );
}

#[test]
fn a_home_a_cargo_builds_with_is_busy() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let root = worktree(&fixture, "wt", true);
    let home = check_in(&fixture, &root);
    // Bare cargo, with the home: it holds the home's package-cache lock for
    // the build, which the gate holds open; no heli lease says anything.
    let gate = fixture.path("gate");
    let started = fixture.path("started");
    let mut child = fixture
        .cargo(["check", "--offline"])
        .current_dir(&root)
        .env("CARGO_HOME", &home)
        .env("HOME_GATE", &gate)
        .env("HOME_STARTED", &started)
        .env_remove("CARGO_TARGET_DIR")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cargo");
    wait_for("the build script to start", || {
        assert!(
            child.try_wait().expect("poll cargo").is_none(),
            "cargo exited before its build script started"
        );
        started.is_file().then_some(())
    });
    let busy = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(
        listed(&busy, &home).expect("listed")["state"],
        "busy",
        "{busy}"
    );
    configure(&fixture, PLAN);
    set_clock(&fixture, now() + 200 * HOUR);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(
        victim(&planned, &home),
        None,
        "busy, whatever its age: {planned}"
    );
    let passed = gc(&fixture, &["gc"]);
    assert_eq!(victim(&passed, &home), None, "{passed}");
    assert!(home.join(".heli-private-home.json").is_file());
    std::fs::write(&gate, "go").expect("open the gate");
    let output = child.wait_with_output().expect("wait for cargo");
    assert_success(&output, "cargo check with the home");
    configure(&fixture, "");
    let free = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(
        listed(&free, &home).expect("listed")["state"],
        "live",
        "{free}"
    );
}
