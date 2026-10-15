//! The eviction ladder (DESIGN.md §6.3): over the budget B, or under the
//! free-space floor F, `cargo heli gc` and the daemon evict what is cheapest
//! to regenerate first, rename each victim into the trash under cargo's
//! lock and delete it afterwards, and never touch a busy directory or
//! anything through a link.
//!
//! Ages come from the injected clock (`[test] clock_ms`) and the times of
//! the files the fixtures leave: a test builds for real, moves the clock
//! on, and sets the times of the files it wants younger or older. Every
//! store is in the fixture's temporary directory, and every build that uses
//! it runs without the harness's `CARGO_TARGET_DIR`, so that the worktree's
//! own `target/` gets the final artifacts.

mod support;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use support::{Fixture, assert_success};

const HOUR: u64 = 3600;
const MB: u64 = 1 << 20;

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

/// `path` with every link and spelling resolved, for comparing places.
fn canonical(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    std::fs::canonicalize(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// `git <args>` in `dir`; panics unless it succeeds.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert_success(&output, &format!("git {}", args.join(" ")));
}

/// Makes `dir` a git repository with one commit of all its files.
fn init_repo(dir: &Path) {
    git(dir, &["init", "--quiet"]);
    git(dir, &["add", "-A"]);
    git(
        dir,
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

/// The store of the fixture's daemon: `<fixture>/store`.
fn store(fixture: &Fixture) -> PathBuf {
    fixture.path("store")
}

/// Configures the fixture's store, with `extra` lines after it in `[store]`.
fn configure(fixture: &Fixture, extra: &str) {
    fixture.configure(&format!(
        "[store]\npaths = ['{}']\n{extra}",
        store(fixture).display()
    ));
}

/// Points cargo's `build-dir` at the store for every workspace in `dir`,
/// as `cargo heli setup` does for a repository.
fn use_store(fixture: &Fixture, dir: &Path) {
    let value = store(fixture).join("{workspace-path-hash}");
    let value = value.to_str().expect("a Unicode path");
    let config = dir.join(".cargo");
    std::fs::create_dir_all(&config).expect("create .cargo");
    std::fs::write(
        config.join("config.toml"),
        format!("[build]\nbuild-dir = \"{}\"\n", value.replace('\\', "\\\\")),
    )
    .expect("write .cargo/config.toml");
}

/// `cargo heli <args>` in `dir`, building into `dir`'s own `target/`.
fn heli_in(fixture: &Fixture, dir: &Path, args: &[&str]) -> Command {
    let mut command = fixture.heli(args.iter().copied());
    command.current_dir(dir).env_remove("CARGO_TARGET_DIR");
    command
}

/// Builds `package` with heli in `dir`, and returns its build directory
/// once the daemon has written its owner manifest.
fn build_in(fixture: &Fixture, dir: &Path, package: &str) -> PathBuf {
    let output = heli_in(fixture, dir, &["build", "--quiet", "-p", package])
        .output()
        .expect("run heli build");
    assert_success(&output, &format!("cargo heli build in {}", dir.display()));
    let workspace = canonical(dir);
    wait_for("the owner manifest", || {
        build_dirs(&store(fixture)).into_iter().find(|build| {
            let Ok(owner) = std::fs::read_to_string(build.join(".heli").join("owner")) else {
                return false;
            };
            let owner: Value = serde_json::from_str(&owner).expect("a JSON manifest");
            owner["workspace"]
                .as_str()
                .and_then(|path| std::fs::canonicalize(path).ok())
                == Some(workspace.clone())
        })
    })
}

/// The build directories in `store`: `<store>/<2 hex digits>/<the rest>`.
fn build_dirs(store: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(store) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.expect("a store entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        for inner in std::fs::read_dir(entry.path()).expect("read a store directory") {
            dirs.push(inner.expect("a build directory").path());
        }
    }
    dirs.sort();
    dirs
}

/// Sets the time of every file under `path` (never through a link) to `at`,
/// seconds since 1970.
fn set_times(path: &Path, at: u64) {
    let time = UNIX_EPOCH + Duration::from_secs(at);
    let metadata = std::fs::symlink_metadata(path).expect("a path to age");
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).expect("read a directory") {
            set_times(&entry.expect("an entry").path(), at);
        }
    } else if metadata.is_file() {
        File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(time))
            .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
    }
}

/// Makes a unit directory `name` in `incremental` (a profile's
/// `incremental/`) holding `bytes` in a session directory, its files
/// written at `at`.
fn fake_unit(incremental: &Path, name: &str, bytes: usize, at: u64) -> PathBuf {
    let unit = incremental.join(name);
    let session = unit.join("s-fake-session");
    std::fs::create_dir_all(&session).expect("create a unit");
    std::fs::write(session.join("dep-graph.bin"), vec![7u8; bytes]).expect("fill it");
    std::fs::write(unit.join("s-fake-session.lock"), "").expect("and its lock");
    set_times(&unit, at);
    unit
}

/// Sets the clock of the fixture's heli to `at`, seconds since 1970.
fn set_clock(fixture: &Fixture, at: u64) {
    fixture.set_clock(at * 1000);
}

/// `cargo heli gc <args> --json`, and the document it prints.
fn gc(fixture: &Fixture, args: &[&str]) -> Value {
    let output = fixture
        .heli(args.iter().copied().chain(["--json"]))
        .output()
        .expect("run heli gc");
    assert_success(&output, &format!("cargo heli {}", args.join(" ")));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("not JSON ({error}): {}", text(&output.stdout));
    });
    assert_eq!(document["schema"], 1, "{document}");
    document
}

/// The victims a gc document names, as (tier, canonical path), in order.
fn victims(document: &Value) -> Vec<(u64, PathBuf)> {
    document["victims"]
        .as_array()
        .unwrap_or_else(|| panic!("no victims: {document}"))
        .iter()
        .map(|victim| {
            let tier = victim["tier"]
                .as_u64()
                .unwrap_or_else(|| panic!("no tier: {victim}"));
            let path = victim["path"]
                .as_str()
                .unwrap_or_else(|| panic!("no path: {victim}"));
            (tier, spelled(Path::new(path)))
        })
        .collect()
}

/// `path` as places compare here: canonical where it exists, else its
/// parent's canonical form and its name.
fn spelled(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => spelled(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Whether anything is left in the trash directories of the store.
fn trash_left(fixture: &Fixture) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(store(fixture).join(".trash")) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.expect("an entry").path())
        .collect()
}

/// The profile directory of a build directory: `debug/`.
fn incremental(build: &Path) -> PathBuf {
    build.join("debug").join("incremental")
}

/// Adds a linked worktree `name` to the repository in `main`, inside it as
/// Claude Code puts them, so the main worktree's Cargo configuration covers
/// it.
fn add_worktree(main: &Path, name: &str) -> PathBuf {
    let linked = main.join(".claude").join("worktrees").join(name);
    git(
        main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            &linked.to_string_lossy(),
        ],
    );
    linked
}

/// Fills a tool cache at `target/tests/trybuild` of worktree `dir` with
/// `bytes`, and returns it.
fn fill_trybuild(dir: &Path, bytes: usize) -> PathBuf {
    let cache = dir.join("target").join("tests").join("trybuild");
    let project = cache.join("project");
    std::fs::create_dir_all(&project).expect("create a tool cache");
    std::fs::write(project.join("out.bin"), vec![1u8; bytes]).expect("fill it");
    cache
}

/// The incremental units in build directory `build`'s `debug/`.
fn units(build: &Path) -> Vec<PathBuf> {
    let mut units: Vec<PathBuf> = std::fs::read_dir(incremental(build))
        .expect("read incremental/")
        .map(|entry| entry.expect("an entry").path())
        .collect();
    units.sort();
    units
}

/// Each tier comes in order, and within a tier the oldest first: units
/// untouched for 24 hours across every build directory, orphans unclaimed
/// for 24 hours, the `incremental/` of directories idle for 6 hours, the
/// tool caches of `target/` directories idle for 6 hours, and whole build
/// directories idle for 7 days. Units already taken leave nothing in an
/// `incremental/`, and nothing inside an orphan already taken is a victim
/// again. An orphan becomes a victim only 24 hours after an eviction pass
/// first found it.
#[test]
fn the_tiers_come_in_order_oldest_first() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "tool_caches = [\"tests/trybuild\"]\n");
    let main = fixture.workspace();
    init_repo(&main);
    use_store(&fixture, &main);
    let clock = now() + 300 * HOUR;
    let mut builds = Vec::new();
    for (name, age) in [
        ("young", 8),
        ("younger-idle", 12),
        ("orphan", 150),
        ("old", 180),
        ("older", 200),
    ] {
        let worktree = add_worktree(&main, name);
        let build = build_in(&fixture, &worktree, "echo-args");
        if age < 24 {
            fill_trybuild(&worktree, 1000);
        }
        set_times(&build, clock - age * HOUR);
        set_times(&worktree.join("target"), clock - age * HOUR);
        builds.push((worktree, build));
    }
    let [young, younger_idle, orphan, old, older] = &builds[..] else {
        unreachable!()
    };
    // Something young in the orphan's incremental/, which goes with it.
    let loose = incremental(&orphan.1).join("loose.bin");
    std::fs::write(&loose, [0u8; 100]).expect("a loose file");
    set_times(&loose, clock - 10 * HOUR);
    git(
        &main,
        &["worktree", "remove", "--force", &orphan.0.to_string_lossy()],
    );
    // A dry run notes nothing: the orphan is unclaimed since no time at all.
    configure(
        &fixture,
        "tool_caches = [\"tests/trybuild\"]\nbudget_mb = 0\norphan_hours = 0\n",
    );
    set_clock(&fixture, clock - 30 * HOUR);
    let unnoted = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(listed(&unnoted, &orphan.1)["state"], "orphan", "{unnoted}");
    assert!(
        !victims(&unnoted).iter().any(|(tier, _)| *tier == 2),
        "{unnoted}"
    );
    // A pass that evicts nothing finds the orphan: it is unclaimed from
    // then on.
    configure(&fixture, "tool_caches = [\"tests/trybuild\"]\n");
    gc(&fixture, &["gc"]);
    configure(
        &fixture,
        "tool_caches = [\"tests/trybuild\"]\nbudget_mb = 0\n",
    );
    set_clock(&fixture, clock - 7 * HOUR);
    let early = gc(&fixture, &["gc", "--dry-run"]);
    assert!(
        !victims(&early).iter().any(|(tier, _)| *tier == 2),
        "unclaimed for 23 hours, an orphan is no victim: {early}"
    );
    // Idle for 1 and 5 hours, the young ones keep their incremental/ and
    // their tool caches.
    assert!(
        victims(&early)
            .iter()
            .all(|(tier, _)| [1, 5].contains(tier)),
        "{early}"
    );

    set_clock(&fixture, clock);
    let unit = |build: &Path| {
        let units: Vec<PathBuf> = units(build)
            .into_iter()
            .filter(|unit| unit.is_dir())
            .collect();
        assert_eq!(units.len(), 1, "{units:?}");
        units[0].clone()
    };
    let trybuild = |worktree: &Path| worktree.join("target").join("tests").join("trybuild");
    let expected = [
        (1, unit(&older.1)),
        (1, unit(&old.1)),
        (1, unit(&orphan.1)),
        (2, orphan.1.clone()),
        (3, incremental(&younger_idle.1)),
        (3, incremental(&young.1)),
        (4, trybuild(&younger_idle.0)),
        (4, trybuild(&young.0)),
        (5, older.1.clone()),
        (5, old.1.clone()),
    ]
    .map(|(tier, path)| (tier, spelled(&path)));
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(victims(&planned), expected, "{planned}");
    for (_, path) in &expected {
        assert!(path.exists(), "--dry-run deleted {}", path.display());
    }

    let done = gc(&fixture, &["gc"]);
    assert_eq!(victims(&done), expected, "{done}");
    for (_, path) in &expected {
        assert!(!path.exists(), "{} is still there", path.display());
    }
    for (worktree, build) in [young, younger_idle] {
        assert!(build.join("debug").join("deps").is_dir(), "deps/ stays");
        assert!(worktree.join("target").join("debug").is_dir());
    }
    let logged: Vec<(u64, PathBuf)> = fixture
        .events_named("evict")
        .iter()
        .map(|event| {
            let path = event["path"].as_str().expect("a path");
            (
                event["tier"].as_u64().expect("a tier"),
                spelled(Path::new(path)),
            )
        })
        .collect();
    assert_eq!(logged, expected);
    assert_eq!(trash_left(&fixture), Vec::<PathBuf>::new());
}

/// A workspace built with heli into the store, everything in it an hour
/// old by the clock `clock`, with four fake units of `unit_mb` megabytes
/// untouched for 100, 90, 80 and 70 hours; returns them, oldest first.
fn four_old_units(fixture: &Fixture, clock: u64, unit_mb: usize) -> Vec<PathBuf> {
    let workspace = fixture.workspace();
    use_store(fixture, &workspace);
    let build = build_in(fixture, &workspace, "echo-args");
    set_times(&build, clock - HOUR);
    set_times(&workspace.join("target"), clock - HOUR);
    [100, 90, 80, 70]
        .into_iter()
        .map(|age| {
            fake_unit(
                &incremental(&build),
                &format!("fake-{age}h"),
                unit_mb * MB as usize,
                clock - age * HOUR,
            )
        })
        .collect()
}

/// What a gc document says every directory takes.
fn used(document: &Value) -> u64 {
    document["used"]
        .as_u64()
        .unwrap_or_else(|| panic!("no used: {document}"))
}

/// Over budget, eviction goes on until everything takes at most 85 % of
/// B, and stops there: of four old units, the oldest two go when the third
/// is not needed.
#[test]
fn eviction_stops_at_85_percent_of_the_budget() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let clock = now() + 200 * HOUR;
    let units = four_old_units(&fixture, clock, 16);
    set_clock(&fixture, clock);
    let ages = "idle_hours = 1000\ndir_hours = 1000\n";
    configure(&fixture, &format!("budget_mb = 1000000\n{ages}"));
    let within = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(victims(&within), [], "within budget, no victims: {within}");
    let before = used(&within);

    // One unit freed brings it under the budget, but not to 85 % of it;
    // two do.
    let unit = 16 * MB;
    let budget_mb = (before - unit).div_ceil(MB);
    let budget = budget_mb * MB;
    let target = budget * 85 / 100;
    assert!(
        before > budget
            && before - unit <= budget
            && before - unit > target
            && before - 2 * unit <= target,
        "the fixture does not fit: used {before}, budget {budget_mb} MB"
    );
    configure(&fixture, &format!("budget_mb = {budget_mb}\n{ages}"));
    let expected = [(1, spelled(&units[0])), (1, spelled(&units[1]))];
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(victims(&planned), expected, "{planned}");
    let done = gc(&fixture, &["gc"]);
    assert_eq!(victims(&done), expected, "{done}");
    assert!(!units[0].exists() && !units[1].exists());
    assert!(units[2].is_dir() && units[3].is_dir(), "eviction stopped");
    let after = gc(&fixture, &["gc", "--dry-run"]);
    assert!(used(&after) <= target, "{after}");
    assert_eq!(victims(&after), [], "{after}");
}

/// Under the floor, a volume with less than F free, eviction starts even
/// within the budget, and takes what the ladder allows on that volume;
/// with F free, it does not start.
#[test]
fn the_floor_starts_eviction_within_the_budget() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let clock = now() + 200 * HOUR;
    let units = four_old_units(&fixture, clock, 1);
    let young = fake_unit(
        units[0].parent().expect("incremental/"),
        "fake-young",
        MB as usize,
        clock - HOUR,
    );
    // A target triple's profile holds units too; a directory without a
    // `-` is no triple.
    let build = units[0]
        .ancestors()
        .nth(3)
        .expect("a build directory")
        .to_path_buf();
    let triple = build.join("x86_64-unknown-test").join("debug");
    let triple_unit = fake_unit(
        &triple.join("incremental"),
        "fake-95h",
        MB as usize,
        clock - 95 * HOUR,
    );
    let not_triple = build.join("notriple").join("debug");
    let not_a_unit = fake_unit(
        &not_triple.join("incremental"),
        "fake-95h",
        MB as usize,
        clock - 95 * HOUR,
    );
    set_clock(&fixture, clock);
    // More than any disk holds free.
    configure(&fixture, "budget_mb = 1000000\nfloor_mb = 1099511627776\n");
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    let mut expected: Vec<(u64, PathBuf)> = units.iter().map(|unit| (1, spelled(unit))).collect();
    expected.insert(1, (1, spelled(&triple_unit)));
    assert_eq!(victims(&planned), expected, "{planned}");
    assert!(not_a_unit.is_dir());
    let volumes = planned["volumes"].as_array().expect("volumes");
    assert!(
        volumes
            .iter()
            .any(|volume| volume["free"].as_u64().is_some()),
        "{planned}"
    );
    gc(&fixture, &["gc"]);
    assert!(units.iter().all(|unit| !unit.exists()));
    assert!(young.is_dir(), "a unit untouched for an hour stays");

    let units = four_old_units(&fixture, clock, 1);
    configure(&fixture, "budget_mb = 1000000\nfloor_mb = 0\n");
    let within = gc(&fixture, &["gc"]);
    assert_eq!(victims(&within), [], "{within}");
    assert!(units.iter().all(|unit| unit.is_dir()));
}

/// The directory a gc document lists at `path`.
fn listed(document: &Value, path: &Path) -> Value {
    let path = spelled(path);
    document["dirs"]
        .as_array()
        .expect("dirs")
        .iter()
        .find(|dir| spelled(Path::new(dir["path"].as_str().expect("a path"))) == path)
        .unwrap_or_else(|| panic!("{} is not listed: {document}", path.display()))
        .clone()
}

/// While a bare Cargo build runs, its directories are busy: neither their units, nor
/// the build directory, idle for a week by its last run, are touched, while
/// another workspace's are evicted; the build completes. Once it is over,
/// the same directory has victims again.
#[test]
fn a_busy_directory_is_never_touched() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let other = fixture.copy_workspace("other");
    let clock = now() + 300 * HOUR;
    let other_build = build_in(&fixture, &other, "echo-args");
    let other_unit = fake_unit(
        &incremental(&other_build),
        "old-unit",
        1000,
        clock - 30 * HOUR,
    );
    let build = build_in(&fixture, &workspace, "echo-args");
    let unit = fake_unit(&incremental(&build), "old-unit", 1000, clock - 200 * HOUR);

    let gate = fixture.path("gate");
    // Exercise Cargo's own compilation locks independently of the stronger
    // managed-runtime activity guard (covered by multi_project_gc).
    let running = fixture
        .cargo(["build", "--quiet", "-p", "gated"])
        .current_dir(&workspace)
        .env_remove("CARGO_TARGET_DIR")
        .env("HELI_FIXTURE_GATE", &gate)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start heli build");
    wait_for("the build directory to be busy", || {
        let listing = gc(&fixture, &["gc", "--dry-run"]);
        (listed(&listing, &build)["state"] == "busy").then_some(())
    });
    set_clock(&fixture, clock);
    configure(&fixture, "budget_mb = 0\n");
    let inside = |path: &PathBuf| {
        path.starts_with(spelled(&build)) || path.starts_with(spelled(&workspace.join("target")))
    };
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert!(
        !victims(&planned).iter().any(|(_, path)| inside(path)),
        "a busy directory has no victims: {planned}"
    );
    let done = gc(&fixture, &["gc"]);
    std::fs::write(&gate, "").expect("open the gate");
    let output = running.wait_with_output().expect("wait for the build");
    assert_success(&output, "bare cargo build -p gated");

    let evicted = victims(&done);
    assert!(
        evicted.contains(&(1, spelled(&other_unit))),
        "the other workspace's unit goes: {done}"
    );
    assert!(
        !evicted.iter().any(|(_, path)| inside(path)),
        "nothing of a busy directory: {done}"
    );
    assert!(unit.is_dir(), "the busy directory's old unit stays");
    assert!(build.join("debug").join("deps").is_dir());

    let after = gc(&fixture, &["gc", "--dry-run"]);
    let planned = victims(&after);
    assert!(planned.contains(&(1, spelled(&unit))), "{after}");
    assert!(planned.contains(&(5, spelled(&build))), "{after}");
}

/// A build directory only bare cargo built in has no owner manifest: heli
/// cannot tell whether its workspace lives. Its incremental units and its
/// `incremental/` can go, but it is never deleted whole, however long it
/// has been idle.
#[test]
fn an_unknown_build_directory_is_never_deleted_whole() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let output = fixture
        .cargo(["build", "--quiet", "-p", "echo-args"])
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run cargo build");
    assert_success(&output, "cargo build");
    let builds = build_dirs(&store(&fixture));
    assert_eq!(builds.len(), 1, "{builds:?}");
    let build = &builds[0];
    let unit = fake_unit(&incremental(build), "old-unit", 1000, now());
    std::fs::write(incremental(build).join("loose.bin"), [0u8; 100]).expect("a loose file");
    set_clock(&fixture, now() + 300 * HOUR);
    configure(&fixture, "budget_mb = 0\n");

    let listing = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(listed(&listing, build)["state"], "unknown", "{listing}");
    let planned = victims(&listing);
    let mut expected: Vec<(u64, PathBuf)> = units(build)
        .iter()
        .filter(|path| path.is_dir())
        .map(|unit| (1, spelled(unit)))
        .collect();
    expected.sort_by(|a, b| a.1.cmp(&b.1));
    let mut tier_1: Vec<(u64, PathBuf)> = planned
        .iter()
        .filter(|(tier, _)| *tier == 1)
        .cloned()
        .collect();
    tier_1.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(tier_1, expected, "{listing}");
    assert!(expected.contains(&(1, spelled(&unit))));
    assert!(
        planned.contains(&(3, spelled(&incremental(build)))),
        "{listing}"
    );
    assert!(
        !planned.iter().any(|(_, path)| *path == spelled(build)),
        "{listing}"
    );

    gc(&fixture, &["gc"]);
    assert!(build.join("debug").join("deps").is_dir(), "deps/ stays");
    assert!(!incremental(build).exists());
}

/// Lock names change between cargo versions. A profile directory with
/// cargo's intermediates and none of the lock files heli knows is of an
/// unknown layout: its directory is busy while anything in it changed
/// within the hour, and only then.
#[test]
fn an_unknown_lock_layout_is_busy_for_an_hour_after_a_change() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "budget_mb = 0\n");
    let build = store(&fixture).join("ab").join("0123456789abcd");
    let profile = build.join("debug");
    std::fs::create_dir_all(profile.join("deps")).expect("create deps/");
    std::fs::write(profile.join("deps").join("libx.rlib"), [0u8; 100]).expect("a unit");
    std::fs::write(profile.join(".cargo-future-lock"), "").expect("an unknown lock");
    let unit = fake_unit(&profile.join("incremental"), "x-1", 1000, now() - 40 * HOUR);
    // The same, with cargo's lock, free.
    let known = store(&fixture).join("cd").join("0123456789abcd");
    let known_profile = known.join("debug");
    std::fs::create_dir_all(known_profile.join("deps")).expect("create deps/");
    std::fs::write(known_profile.join("deps").join("libx.rlib"), [0u8; 100]).expect("a unit");
    std::fs::write(known_profile.join(".cargo-build-lock"), "").expect("cargo's lock");
    let known_unit = fake_unit(
        &known_profile.join("incremental"),
        "x-1",
        1000,
        now() - 40 * HOUR,
    );
    // The rlibs were written now.
    set_clock(&fixture, now() + 59 * 60);
    let listing = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(listed(&listing, &build)["state"], "busy", "{listing}");
    assert_eq!(listed(&listing, &known)["state"], "unknown", "{listing}");
    assert_eq!(victims(&listing), [(1, spelled(&known_unit))], "{listing}");
    gc(&fixture, &["gc"]);
    assert!(unit.is_dir(), "a busy directory's unit stays");
    assert!(!known_unit.exists());

    set_clock(&fixture, now() + 61 * 60);
    let listing = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(listed(&listing, &build)["state"], "unknown", "{listing}");
    assert_eq!(victims(&listing), [(1, spelled(&unit))], "{listing}");
}

/// Links `link` to the directory `to`: a junction on Windows, which needs
/// no privilege, a symlink elsewhere.
fn link_dir(to: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(to)
            .output()
            .expect("run mklink");
        assert_success(&output, "mklink /J");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(to, link).expect("make a symlink");
}

/// A directory outside every store and worktree, holding `keep.txt`.
fn outside(fixture: &Fixture, name: &str) -> PathBuf {
    let dir = fixture.path(name);
    std::fs::create_dir_all(dir.join("fake-unit")).expect("create a directory outside");
    std::fs::write(dir.join("keep.txt"), "keep").expect("a file to keep");
    std::fs::write(dir.join("fake-unit").join("data.bin"), [0u8; 100]).expect("and another");
    set_times(&dir, now() - 1000 * HOUR);
    dir
}

/// Eviction never goes through a link (a junction on Windows, a symlink
/// elsewhere): a link inside a unit or a build directory that is evicted is
/// removed, not followed; an `incremental/` that is a link has no victims;
/// a tool cache that is a link is none. Every file the links point to
/// survives.
#[test]
fn eviction_never_goes_through_a_link() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "tool_caches = [\"tests/trybuild\"]\n");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let other = fixture.copy_workspace("other");
    let clock = now() + 300 * HOUR;
    let build = build_in(&fixture, &workspace, "echo-args");
    let other_build = build_in(&fixture, &other, "echo-args");

    let in_unit = outside(&fixture, "outside-unit");
    let unit = fake_unit(&incremental(&build), "linked-unit", 1000, now());
    link_dir(&in_unit, &unit.join("s-fake-session").join("link"));
    let in_build = outside(&fixture, "outside-build");
    link_dir(&in_build, &build.join("debug").join("link"));
    let as_incremental = outside(&fixture, "outside-incremental");
    std::fs::remove_dir_all(incremental(&other_build)).expect("remove incremental/");
    link_dir(&as_incremental, &incremental(&other_build));
    let as_cache = outside(&fixture, "outside-cache");
    let tests = other.join("target").join("tests");
    std::fs::create_dir_all(&tests).expect("create target/tests");
    link_dir(&as_cache, &tests.join("trybuild"));

    set_clock(&fixture, clock);
    configure(
        &fixture,
        "tool_caches = [\"tests/trybuild\"]\nbudget_mb = 0\n",
    );
    let through_a_link = |document: &Value| {
        for (tier, path) in victims(document) {
            assert!(
                !path.starts_with(spelled(&incremental(&other_build)))
                    && !path.starts_with(spelled(&tests.join("trybuild"))),
                "tier {tier} victim {} is reached through a link: {document}",
                path.display()
            );
        }
    };
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    through_a_link(&planned);
    let linked_unit = planned["victims"]
        .as_array()
        .expect("victims")
        .iter()
        .find(|victim| {
            victim["tier"] == 1
                && victim["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("linked-unit"))
        })
        .unwrap_or_else(|| panic!("no linked unit: {planned}"))
        .clone();
    assert_eq!(
        linked_unit["bytes"], 1000,
        "a link counts nothing: {linked_unit}"
    );
    let done = gc(&fixture, &["gc"]);
    through_a_link(&done);
    let evicted = victims(&done);
    assert!(evicted.contains(&(1, spelled(&unit))), "{done}");
    assert!(evicted.contains(&(5, spelled(&build))), "{done}");
    assert!(evicted.contains(&(5, spelled(&other_build))), "{done}");
    assert!(!build.exists() && !other_build.exists());
    for dir in [&in_unit, &in_build, &as_incremental, &as_cache] {
        assert_eq!(
            std::fs::read_to_string(dir.join("keep.txt"))
                .ok()
                .as_deref(),
            Some("keep"),
            "{} lost a file",
            dir.display()
        );
        assert!(dir.join("fake-unit").join("data.bin").is_file());
    }
    assert!(
        std::fs::symlink_metadata(tests.join("trybuild")).is_ok(),
        "a tool cache that is a link stays"
    );
    assert_eq!(trash_left(&fixture), Vec::<PathBuf>::new());
}

/// A large victim leaves its directory at once, renamed into the trash
/// under cargo's lock, and is deleted after the lock is let go: a cargo
/// that starts in that directory as soon as the victim is gone never waits
/// for its lock, and builds, while the deletion may still go on.
#[test]
fn deleting_a_large_victim_does_not_hold_up_cargo() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let clock = now() + 300 * HOUR;
    let build = build_in(&fixture, &workspace, "echo-args");
    set_times(&build, clock - HOUR);
    set_times(&workspace.join("target"), clock - HOUR);
    // 20 000 files: seconds to delete.
    let large = incremental(&build).join("large-1a2b3c");
    for part in 0..20 {
        let dir = large.join(format!("s-{part}"));
        std::fs::create_dir_all(&dir).expect("create a session directory");
        for file in 0..1000 {
            std::fs::write(dir.join(format!("{file}.o")), [0u8; 1024]).expect("fill it");
        }
    }
    set_times(&large, clock - 200 * HOUR);
    set_clock(&fixture, clock);
    configure(&fixture, "budget_mb = 0\n");

    let gc = fixture
        .heli(["gc", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start heli gc");
    // It leaves whole, never seen half deleted.
    let deadline = Instant::now() + Duration::from_secs(120);
    while large.exists() {
        let partial = (0..20).any(|part| !large.join(format!("s-{part}")).exists());
        assert!(
            !(partial && large.exists()),
            "the victim is deleted in place"
        );
        assert!(Instant::now() < deadline, "the victim never left");
        std::thread::sleep(Duration::from_millis(5));
    }
    let cargo = fixture
        .cargo(["build", "-p", "echo-args"])
        .current_dir(&workspace)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run cargo build");
    assert_success(&cargo, "cargo build while the victim is deleted");
    let stderr = text(&cargo.stderr);
    // The package cache's lock, in CARGO_HOME, is every cargo's on the
    // host: the tests that run beside this one, other agents. Waiting for
    // it says nothing of gc. Any other lock is the build directory's or the
    // artifact directory's (`Blocking waiting for file lock on build
    // directory`, `… on artifact directory`, with cargo 1.99).
    assert!(
        !stderr.lines().any(|line| {
            line.contains("waiting for file lock")
                && line.trim() != "Blocking waiting for file lock on package cache"
        }),
        "cargo waited for gc: {stderr}"
    );

    let output = gc.wait_with_output().expect("wait for heli gc");
    assert_success(&output, "cargo heli gc");
    let done: Value = serde_json::from_slice(&output.stdout).expect("a JSON document");
    assert_eq!(victims(&done), [(1, spelled(&large))], "{done}");
    assert_eq!(trash_left(&fixture), Vec::<PathBuf>::new());
}

/// The daemon evicts by itself: once a run ends, its figures are over the
/// budget, and it makes a pass. Each eviction goes to the event log,
/// `status` shows the pass and the budget, and the daemon empties the
/// trash.
#[test]
fn the_daemon_evicts_when_over_budget() {
    let fixture = Fixture::new("basic");
    set_clock(&fixture, now() + 100 * HOUR);
    configure(
        &fixture,
        "budget_mb = 0\nidle_hours = 1000\ndir_hours = 1000\n",
    );
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let build = build_in(&fixture, &workspace, "echo-args");

    let evictions = wait_for("the daemon's evictions", || {
        let evictions = fixture.events_named("evict");
        (!evictions.is_empty()).then_some(evictions)
    });
    let status = fixture.wait_for_status("the daemon's last pass", |status| {
        status["last_gc"]["evicted"]
            .as_array()
            .is_some_and(|evicted| !evicted.is_empty())
    });
    let shown: Vec<(u64, PathBuf)> = status["last_gc"]["evicted"]
        .as_array()
        .expect("evicted")
        .iter()
        .map(|evicted| {
            let path = evicted["path"].as_str().expect("a path");
            assert!(evicted["bytes"].as_u64().is_some_and(|bytes| bytes > 0));
            (
                evicted["tier"].as_u64().expect("a tier"),
                spelled(Path::new(path)),
            )
        })
        .collect();
    let logged: Vec<(u64, PathBuf)> = evictions
        .iter()
        .map(|event| {
            let path = event["path"].as_str().expect("a path");
            (
                event["tier"].as_u64().expect("a tier"),
                spelled(Path::new(path)),
            )
        })
        .collect();
    assert_eq!(shown, logged, "{status}");
    assert!(!logged.is_empty());
    for (tier, path) in &logged {
        assert_eq!(*tier, 1, "{logged:?}");
        assert_eq!(path.parent(), Some(spelled(&incremental(&build)).as_path()));
        assert!(!path.exists(), "{} is still there", path.display());
    }
    assert!(status["last_gc"]["at"].as_u64().is_some(), "{status}");
    let stores = status["disk"]["stores"].as_array().expect("stores");
    assert!(stores.iter().all(|store| store["budget"] == 0), "{status}");
    let output = fixture.heli(["status"]).output().expect("run heli status");
    assert_success(&output, "cargo heli status");
    let stdout = text(&output.stdout);
    let line = "gc      ";
    assert!(
        stdout
            .lines()
            .any(|shown| shown.starts_with(line)
                && shown.contains(&format!("evicted {}", logged.len()))),
        "{stdout}"
    );
    assert!(build.join("debug").join("deps").is_dir(), "deps/ stays");
    wait_for("the daemon to empty the trash", || {
        trash_left(&fixture).is_empty().then_some(())
    });
}

/// A trash that is a link is never used: nothing is renamed into it, and
/// nothing it points to is deleted. The victim is skipped, and says why.
#[test]
fn a_trash_that_is_a_link_is_never_used() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let build = build_in(&fixture, &workspace, "echo-args");
    let clock = now() + 100 * HOUR;
    set_times(&build, clock - HOUR);
    let old = fake_unit(&incremental(&build), "old-1a2b3c", 1000, clock - 30 * HOUR);
    let elsewhere = outside(&fixture, "outside-trash");
    link_dir(&elsewhere, &store(&fixture).join(".trash"));
    set_clock(&fixture, clock);
    configure(&fixture, "budget_mb = 0\n");

    let done = gc(&fixture, &["gc"]);
    assert_eq!(victims(&done), [], "{done}");
    let skipped = done["skipped"].as_array().expect("skipped");
    assert_eq!(skipped.len(), 1, "{done}");
    assert_eq!(
        spelled(Path::new(skipped[0]["path"].as_str().expect("a path"))),
        spelled(&old)
    );
    assert!(
        skipped[0]["why"]
            .as_str()
            .is_some_and(|why| why.contains("trash")),
        "{done}"
    );
    assert!(old.is_dir(), "the victim stays where it was");
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("keep.txt"))
            .ok()
            .as_deref(),
        Some("keep")
    );
    assert!(elsewhere.join("fake-unit").join("data.bin").is_file());
}

/// With a budget or a floor, the daemon notes an orphan when it first
/// finds one, as it looks after a run: from then on the orphan counts as
/// unclaimed (a dry run notes nothing itself). Without either, it writes
/// no note.
#[test]
fn the_daemon_finds_orphans() {
    let fixture = Fixture::new("basic");
    let clock = now() + 10 * HOUR;
    set_clock(&fixture, clock);
    configure(&fixture, "");
    let main = fixture.workspace();
    init_repo(&main);
    use_store(&fixture, &main);
    let linked = add_worktree(&main, "linked");
    let orphan = build_in(&fixture, &linked, "echo-args");
    // The main worktree has a build directory of its own, so that its runs
    // below recycle nothing (DESIGN.md §6.4).
    build_in(&fixture, &main, "echo-args");
    git(
        &main,
        &["worktree", "remove", "--force", &linked.to_string_lossy()],
    );
    // For gc only: the daemon read its configuration as it started.
    let to_plan = "budget_mb = 0\norphan_hours = 0\nidle_hours = 1000\n";
    configure(&fixture, to_plan);
    // The daemon looks once this run ends.
    build_in(&fixture, &main, "echo-args");
    std::thread::sleep(Duration::from_secs(2));
    let unnoted = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(listed(&unnoted, &orphan)["state"], "orphan", "{unnoted}");
    assert!(
        !victims(&unnoted).iter().any(|(tier, _)| *tier == 2),
        "without a budget, the daemon notes nothing: {unnoted}"
    );

    fixture.kill_daemon();
    configure(&fixture, "budget_mb = 1000000\n");
    build_in(&fixture, &main, "echo-args");
    configure(&fixture, to_plan);
    let found = wait_for("the daemon to find the orphan", || {
        let planned = gc(&fixture, &["gc", "--dry-run"]);
        victims(&planned)
            .contains(&(2, spelled(&orphan)))
            .then_some(planned)
    });
    let victim = found["victims"]
        .as_array()
        .expect("victims")
        .iter()
        .find(|victim| victim["tier"] == 2)
        .expect("the orphan")
        .clone();
    assert_eq!(victim["age"], 0, "unclaimed since the clock's now: {found}");
}

/// Under the floor, the daemon evicts by itself too.
#[test]
fn the_daemon_evicts_under_the_floor() {
    let fixture = Fixture::new("basic");
    set_clock(&fixture, now() + 100 * HOUR);
    // More than any disk holds free.
    configure(
        &fixture,
        "floor_mb = 1099511627776\nidle_hours = 1000\ndir_hours = 1000\n",
    );
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let build = build_in(&fixture, &workspace, "echo-args");
    let evictions = wait_for("the daemon's evictions", || {
        let evictions = fixture.events_named("evict");
        (!evictions.is_empty()).then_some(evictions)
    });
    for event in &evictions {
        assert_eq!(event["tier"], 1, "{evictions:?}");
        let path = spelled(Path::new(event["path"].as_str().expect("a path")));
        assert_eq!(path.parent(), Some(spelled(&incremental(&build)).as_path()));
    }
}

/// A worktree that comes back at the same place makes its build directory
/// live again, and it loses its note: removed again, it is unclaimed from
/// the next pass on, not from the first time.
#[test]
fn an_orphan_that_comes_back_starts_over() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let main = fixture.workspace();
    init_repo(&main);
    use_store(&fixture, &main);
    let linked = add_worktree(&main, "linked");
    let build = build_in(&fixture, &linked, "echo-args");
    let remove = || {
        git(
            &main,
            &["worktree", "remove", "--force", &linked.to_string_lossy()],
        );
    };
    remove();
    let clock = now() + 10 * HOUR;
    set_clock(&fixture, clock);
    gc(&fixture, &["gc"]);
    assert_eq!(add_worktree(&main, "linked"), linked);
    set_clock(&fixture, clock + HOUR);
    let back = gc(&fixture, &["gc"]);
    assert_eq!(listed(&back, &build)["state"], "live", "{back}");
    remove();

    let to_plan = "budget_mb = 0\nunit_hours = 1000\nidle_hours = 1000\n";
    configure(&fixture, to_plan);
    set_clock(&fixture, clock + 25 * HOUR);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(listed(&planned, &build)["state"], "orphan", "{planned}");
    assert_eq!(
        victims(&planned),
        [],
        "unclaimed for no time yet: {planned}"
    );
    configure(&fixture, "");
    gc(&fixture, &["gc"]);
    configure(&fixture, to_plan);
    set_clock(&fixture, clock + 49 * HOUR);
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(victims(&planned), [(2, spelled(&build))], "{planned}");
}

/// What a `gc` that was stopped left in a trash is no part of any
/// directory's size, counts as free on its volume, and is deleted by the
/// next `gc`.
#[test]
fn leftovers_in_the_trash_are_free_and_deleted() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    build_in(&fixture, &workspace, "echo-args");
    let target = workspace.join("target");
    wait_for("the target directory to be registered", || {
        let listing = gc(&fixture, &["gc", "--dry-run"]);
        listing["dirs"]
            .as_array()
            .is_some_and(|dirs| dirs.iter().any(|dir| dir["kind"] == "target"))
            .then_some(())
    });
    let before = gc(&fixture, &["gc", "--dry-run"]);
    let size = |document: &Value| listed(document, &target)["size"].as_u64().expect("a size");
    let store_trash = store(&fixture).join(".trash").join("leftover");
    let target_trash = target.join(".heli-trash").join("leftover");
    for trash in [&store_trash, &target_trash] {
        std::fs::create_dir_all(trash).expect("create a leftover");
        std::fs::write(trash.join("data.bin"), vec![0u8; 100_000]).expect("fill it");
    }
    let after = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(size(&after), size(&before), "{after}");
    assert_eq!(used(&after), used(&before), "{after}");
    let pending: u64 = after["volumes"]
        .as_array()
        .expect("volumes")
        .iter()
        .map(|volume| volume["pending"].as_u64().expect("pending"))
        .sum();
    assert_eq!(pending, 200_000, "{after}");
    assert!(store_trash.is_dir() && target_trash.is_dir());

    gc(&fixture, &["gc"]);
    assert!(!store_trash.exists() && !target_trash.exists());
}

/// A registered `target/` directory that holds cargo's intermediates
/// itself (no `build-dir`) loses its incremental units and its
/// `incremental/`, never itself whole, however long it has been idle; its
/// victims go through a trash of its own, which is gone once `gc` is done.
#[test]
fn a_target_directory_loses_only_its_incremental_caches() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let workspace = fixture.workspace();
    let output = heli_in(
        &fixture,
        &workspace,
        &["build", "--quiet", "-p", "echo-args"],
    )
    .output()
    .expect("run heli build");
    assert_success(&output, "cargo heli build");
    let target = workspace.join("target");
    wait_for("the target directory to be registered", || {
        let listing = gc(&fixture, &["gc", "--dry-run"]);
        listing["dirs"]
            .as_array()
            .is_some_and(|dirs| dirs.iter().any(|dir| dir["kind"] == "target"))
            .then_some(())
    });
    let units = units(&target);
    assert!(!units.is_empty());
    std::fs::write(incremental(&target).join("loose.bin"), [0u8; 10]).expect("a loose file");
    set_clock(&fixture, now() + 300 * HOUR);
    configure(&fixture, "budget_mb = 0\n");

    let mut expected: Vec<(u64, PathBuf)> = units.iter().map(|unit| (1, spelled(unit))).collect();
    expected.sort_by(|a, b| a.1.cmp(&b.1));
    expected.push((3, spelled(&incremental(&target))));
    let in_order = |document: &Value| {
        let mut victims = victims(document);
        let units = victims.len().saturating_sub(1);
        victims[..units].sort_by(|a, b| a.1.cmp(&b.1));
        victims
    };
    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(in_order(&planned), expected, "{planned}");
    let done = gc(&fixture, &["gc"]);
    assert_eq!(in_order(&done), expected, "{done}");
    assert_eq!(done["skipped"], serde_json::json!([]), "{done}");
    assert!(target.join("debug").join("deps").is_dir(), "deps/ stays");
    assert!(
        !target.join(".heli-trash").exists(),
        "the trash is gone once empty"
    );
}

/// A tool cache that holds a cargo lock of its own (a nested build's) is
/// skipped while that lock is held, and evicted once it is let go.
#[test]
fn a_tool_cache_whose_lock_is_held_is_skipped() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "tool_caches = [\"tests/trybuild\"]\n");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    build_in(&fixture, &workspace, "echo-args");
    let cache = fill_trybuild(&workspace, 1000);
    let nested = cache.join("debug");
    std::fs::create_dir_all(&nested).expect("create a nested profile");
    let lock = File::create(nested.join(".cargo-lock")).expect("create its lock");
    lock.lock().expect("take its lock");
    set_clock(&fixture, now() + 10 * HOUR);
    configure(
        &fixture,
        "tool_caches = [\"tests/trybuild\"]\nbudget_mb = 0\nunit_hours = 1000\ndir_hours = 1000\n",
    );

    let held = gc(&fixture, &["gc"]);
    let skipped = held["skipped"].as_array().expect("skipped");
    assert!(
        skipped.iter().any(|victim| victim["tier"] == 4
            && spelled(Path::new(victim["path"].as_str().expect("a path"))) == spelled(&cache)
            && victim["why"]
                .as_str()
                .is_some_and(|why| why.contains("busy"))),
        "{held}"
    );
    assert!(cache.join("project").join("out.bin").is_file());

    drop(lock);
    let done = gc(&fixture, &["gc"]);
    assert!(victims(&done).contains(&(4, spelled(&cache))), "{done}");
    assert!(!cache.exists());
}

/// Over budget, `gc` evicts incremental units untouched for 24 hours,
/// nothing younger; `--dry-run` names the same victims, each with its tier,
/// and deletes nothing. Each eviction goes to the event log, and the trash
/// is empty once `gc` is done.
#[test]
fn over_budget_gc_evicts_units_untouched_for_a_day() {
    let fixture = Fixture::new("basic");
    configure(&fixture, "");
    let workspace = fixture.workspace();
    use_store(&fixture, &workspace);
    let build = build_in(&fixture, &workspace, "echo-args");
    let clock = now() + 100 * HOUR;
    // The build is an hour old, a fake unit 30 hours, another 23.
    set_times(&build, clock - HOUR);
    let old = fake_unit(
        &incremental(&build),
        "old-1a2b3c",
        MB as usize,
        clock - 30 * HOUR,
    );
    let young = fake_unit(
        &incremental(&build),
        "young-1a2b3c",
        MB as usize,
        clock - 23 * HOUR,
    );
    set_clock(&fixture, clock);
    configure(&fixture, "budget_mb = 0\n");

    let planned = gc(&fixture, &["gc", "--dry-run"]);
    assert_eq!(victims(&planned), [(1, spelled(&old))], "{planned}");
    assert_eq!(
        planned["victims"][0]["reason"], "incremental unit untouched for 30 h",
        "{planned}"
    );
    assert!(old.is_dir() && young.is_dir(), "--dry-run deletes nothing");
    let output = fixture
        .heli(["gc", "--dry-run"])
        .output()
        .expect("run heli gc --dry-run");
    assert_success(&output, "cargo heli gc --dry-run");
    let stdout = text(&output.stdout);
    let shown = old.display().to_string();
    assert!(
        stdout.lines().any(|line| line.starts_with("evict ")
            && line.contains("tier 1")
            && line.contains(&shown)
            && line.contains("(incremental unit untouched for 30 h)")),
        "{stdout}"
    );
    assert!(old.is_dir(), "--dry-run deletes nothing");

    let done = gc(&fixture, &["gc"]);
    assert_eq!(victims(&done), [(1, spelled(&old))], "{done}");
    assert!(!old.exists(), "the old unit is evicted");
    assert!(young.is_dir(), "a unit untouched for 23 hours stays");
    assert!(build.join("debug").join("deps").is_dir(), "deps/ stays");
    assert_eq!(trash_left(&fixture), Vec::<PathBuf>::new());
    let evictions = fixture.events_named("evict");
    assert_eq!(evictions.len(), 1, "{evictions:?}");
    assert_eq!(evictions[0]["tier"], 1, "{evictions:?}");
    assert_eq!(
        spelled(Path::new(evictions[0]["path"].as_str().expect("a path"))),
        spelled(&old)
    );
    assert_eq!(evictions[0]["bytes"].as_u64(), Some(MB), "{evictions:?}");
}
