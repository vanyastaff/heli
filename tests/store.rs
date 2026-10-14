//! The build store (DESIGN.md §6.1, §6.2): `cargo heli setup` points cargo's
//! `build.build-dir` at a store, the daemon writes an owner manifest into
//! each build directory and keeps their sizes, and `cargo heli gc
//! --dry-run` lists every directory with its state and size.
//!
//! Every store here is in the fixture's temporary directory (`[store]
//! paths` in its configuration), every `setup` runs in its temporary home
//! ([`Fixture::setup`], whose Cargo home is there too), and every build that
//! should use the store runs without the harness's `CARGO_TARGET_DIR`, so
//! that the worktree's own `target/` gets the final artifacts.

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::{Fixture, assert_success};

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
/// 30 s, naming `what` it waited for.
fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
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

/// The owner manifest of build directory `dir`, if it has one.
fn owner(dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(dir.join(".heli").join("owner")).ok()?;
    Some(serde_json::from_str(&text).unwrap_or_else(|error| panic!("{text}: {error}")))
}

/// A whole number of seconds from a manifest.
fn seconds(owner: &Value, field: &str) -> u64 {
    owner[field]
        .as_u64()
        .unwrap_or_else(|| panic!("no {field} in {owner}"))
}

/// The comment setup puts before the line it adds to a Cargo configuration.
const MARKER: &str =
    "# Added by `cargo heli setup`: cargo keeps its intermediates in heli's build store.";

/// `git <args>` in `dir`; panics unless it succeeds, and returns its stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert_success(&output, &format!("git {}", args.join(" ")));
    text(&output.stdout)
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

/// Configures `<fixture>/store` as the build store, with `extra` lines after
/// it, and returns the store's path.
fn configure_store(fixture: &Fixture, extra: &str) -> PathBuf {
    let store = fixture.path("store");
    fixture.configure(&format!(
        "[store]\npaths = ['{}']\n{extra}",
        store.display()
    ));
    store
}

/// Runs `command` with `answers` on its stdin, then end of input.
fn answered(mut command: Command, answers: &str) -> Output {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("run heli setup");
    child
        .stdin
        .take()
        .expect("a stdin pipe")
        .write_all(answers.as_bytes())
        .expect("answer");
    child.wait_with_output().expect("wait for heli setup")
}

/// The files setup asked to write (`Write <path>? [y/N]`), in order. Paths
/// setup finds from its working directory are spelled as the system
/// resolves them (on macOS `/private/var`), so compare those canonically.
fn asked(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| {
            let (path, _) = line.strip_prefix("Write ")?.split_once("? [y/N]")?;
            Some(PathBuf::from(path))
        })
        .collect()
}

/// The `build-dir` setup writes for `store`, as a TOML basic string.
fn build_dir_value(store: &Path) -> String {
    let value = store.join("{workspace-path-hash}");
    let value = value.to_str().expect("a Unicode path");
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A Cargo configuration that setup created to hold `build-dir` alone.
fn fresh_config(store: &Path) -> String {
    format!(
        "[build]\n{MARKER}\nbuild-dir = {}\n",
        build_dir_value(store)
    )
}

/// The build directories in `store`: `<store>/<2 hex digits>/<the rest>`,
/// as cargo lays out `{workspace-path-hash}`.
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

/// The file name of binary `name` on this platform.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// `cargo heli <args>` in `dir`, building into `dir`'s own `target/`.
fn heli_in(fixture: &Fixture, dir: &Path, args: &[&str]) -> Command {
    let mut command = fixture.heli(args.iter().copied());
    command.current_dir(dir).env_remove("CARGO_TARGET_DIR");
    command
}

/// Set up for one repository, a build keeps its intermediates in the store
/// and its final artifacts in the worktree's `target/`. The setting is the
/// repository's own `.cargo/config.toml`: the user's Cargo configuration is
/// offered too, declined, and left as it was.
#[test]
fn a_repository_set_up_builds_into_the_store() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let workspace = fixture.workspace();
    init_repo(&workspace);

    let output = answered(fixture.setup(None, Vec::<String>::new()), "y\nn\n");
    assert_success(&output, "cargo heli setup");
    let stdout = text(&output.stdout);
    let repository_config = workspace.join(".cargo").join("config.toml");
    let user_config = fixture.home().join(".cargo").join("config.toml");
    let asked = asked(&stdout);
    // The third is the repository's `post-checkout` hook (tests/recycle.rs),
    // declined by the end of input.
    assert_eq!(asked.len(), 3, "three questions: {stdout}");
    assert_eq!(asked[1], user_config, "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&repository_config).expect("the repository's configuration"),
        fresh_config(&store)
    );
    assert_eq!(canonical(&asked[0]), canonical(&repository_config));
    assert!(
        !user_config.exists(),
        "the user's configuration is untouched"
    );

    let output = heli_in(
        &fixture,
        &workspace,
        &["build", "--quiet", "-p", "echo-args"],
    )
    .output()
    .expect("run heli build");
    assert_success(&output, "cargo heli build");
    assert_placed(&store, &workspace);
}

/// Panics unless `echo-args`, built in `workspace`, has its binary in the
/// workspace's `target/` and its intermediates in `store`, in the store's
/// only build directory.
fn assert_placed(store: &Path, workspace: &Path) {
    let target = workspace.join("target").join("debug");
    assert!(
        target.join(exe("echo-args")).is_file(),
        "the binary is in the worktree's target/"
    );
    assert!(
        !target.join("deps").exists() && !target.join("incremental").exists(),
        "no intermediates in the worktree's target/"
    );
    let dirs = build_dirs(store);
    assert_eq!(dirs.len(), 1, "one build directory: {dirs:?}");
    let deps = dirs[0].join("debug").join("deps");
    let built = std::fs::read_dir(&deps)
        .expect("the store's deps/")
        .any(|entry| {
            let name = entry.expect("an entry").file_name();
            name.to_string_lossy().starts_with("echo_args")
        });
    assert!(built, "echo-args was compiled in {}", deps.display());
}

/// Set up for every project, outside any repository, the user's Cargo
/// configuration gets `build-dir` under its own `[build]` table, everything
/// else in it kept, and a build that reads it uses the store. Run again,
/// setup finds it set and asks nothing.
#[test]
fn every_project_set_up_builds_into_the_store() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let workspace = fixture.workspace();
    let cargo_home = fixture.home().join(".cargo");
    std::fs::create_dir_all(&cargo_home).expect("create the Cargo home");
    let user_config = cargo_home.join("config.toml");
    std::fs::write(
        &user_config,
        "# the user's own\n[build]  # theirs too\njobs = 2\n\n[env]\nFOO = \"bar\"\n",
    )
    .expect("write the user's configuration");

    let output = answered(fixture.setup(None, Vec::<String>::new()), "y\n");
    assert_success(&output, "cargo heli setup");
    let stdout = text(&output.stdout);
    let none = stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("Cargo configuration for this repository: none (")?
                .strip_suffix(" is not in a git repository).")
        })
        .unwrap_or_else(|| panic!("no repository to offer: {stdout}"));
    assert_eq!(canonical(none), canonical(&workspace));
    assert_eq!(
        asked(&stdout),
        std::slice::from_ref(&user_config),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&user_config).expect("the user's configuration"),
        format!(
            "# the user's own\n[build]  # theirs too\n{MARKER}\nbuild-dir = {}\njobs = 2\n\n\
             [env]\nFOO = \"bar\"\n",
            build_dir_value(&store)
        )
    );

    let output = heli_in(
        &fixture,
        &workspace,
        &["build", "--quiet", "-p", "echo-args"],
    )
    .env("CARGO_HOME", &cargo_home)
    .output()
    .expect("run heli build");
    assert_success(&output, "cargo heli build");
    assert_placed(&store, &workspace);

    let output = answered(fixture.setup(None, Vec::<String>::new()), "y\n");
    assert_success(&output, "cargo heli setup, again");
    let stdout = text(&output.stdout);
    assert!(!stdout.contains("[y/N]"), "nothing left to ask: {stdout}");
    let done = format!(
        "Cargo configuration for every project {} already sets build.build-dir to this.",
        user_config.display()
    );
    assert!(stdout.lines().any(|line| line == done), "{stdout}");
}

/// Sets up the repository whose main worktree is `dir`, `<fixture>/store`
/// as its store; the user's Cargo configuration is declined.
fn set_up_repository(fixture: &Fixture, dir: &Path) {
    let mut setup = fixture.setup(None, Vec::<String>::new());
    setup.current_dir(dir);
    let output = answered(setup, "y\nn\n");
    assert_success(&output, "cargo heli setup");
    assert!(dir.join(".cargo").join("config.toml").is_file());
}

/// Builds `echo-args` with heli in `dir`.
fn build_in(fixture: &Fixture, dir: &Path) {
    let output = heli_in(fixture, dir, &["build", "--quiet", "-p", "echo-args"])
        .output()
        .expect("run heli build");
    assert_success(&output, &format!("cargo heli build in {}", dir.display()));
}

/// The owner manifests of the build directories in `store`, once there are
/// `count` of them, by the canonical path of their workspace.
fn owners(store: &Path, count: usize) -> Vec<(PathBuf, Value)> {
    wait_for(&format!("{count} owner manifests"), || {
        let owners: Vec<(PathBuf, Value)> = build_dirs(store)
            .iter()
            .filter_map(|dir| owner(dir))
            .map(|owner| {
                let workspace = owner["workspace"].as_str().expect("a workspace path");
                (canonical(workspace), owner)
            })
            .collect();
        (owners.len() == count).then_some(owners)
    })
}

/// A run in a workspace leaves an owner manifest in its build directory: the
/// workspace, its repository (the git common directory, shared by the
/// worktrees of one repository), when the manifest was created and when
/// the last run started. A later run updates only the last.
#[test]
fn each_build_directory_names_its_owner() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let main = fixture.workspace();
    let other = fixture.copy_workspace("other");
    init_repo(&main);
    init_repo(&other);
    set_up_repository(&fixture, &main);
    set_up_repository(&fixture, &other);
    // Inside the main worktree, as Claude Code puts its worktrees, so its
    // configuration covers this one too.
    let linked = main.join(".claude").join("worktrees").join("linked");
    git(
        &main,
        &["worktree", "add", "--quiet", &linked.to_string_lossy()],
    );

    let before = now();
    for dir in [&main, &linked, &other] {
        build_in(&fixture, dir);
    }
    let owners = owners(&store, 3);
    let after = now();
    let find = |dir: &Path| {
        let dir = canonical(dir);
        owners
            .iter()
            .find(|(workspace, _)| *workspace == dir)
            .map(|(_, owner)| owner.clone())
            .unwrap_or_else(|| panic!("no manifest for {}: {owners:?}", dir.display()))
    };
    let repository = |owner: &Value| canonical(owner["repository"].as_str().expect("a repository"));
    for (dir, repo) in [(&main, &main), (&linked, &main), (&other, &other)] {
        let owner = find(dir);
        assert_eq!(repository(&owner), canonical(repo.join(".git")), "{owner}");
        // As people write paths: on Windows without `\\?\`, and with `\`.
        let spelled = owner["repository"].as_str().expect("a repository");
        if cfg!(windows) {
            assert!(
                !spelled.starts_with(r"\\?\") && !spelled.contains('/'),
                "{owner}"
            );
        }
        let (created, last) = (seconds(&owner, "created_at"), seconds(&owner, "last_run"));
        assert!(
            before <= created && created <= last && last <= after,
            "{owner}"
        );
    }

    let first = find(&main);
    std::thread::sleep(Duration::from_millis(1100));
    build_in(&fixture, &main);
    let later = wait_for("the last run to move", || {
        let owner = find_owner(&store, &main);
        (seconds(&owner, "last_run") > seconds(&first, "last_run")).then_some(owner)
    });
    assert_eq!(later["created_at"], first["created_at"], "{later}");
    assert_eq!(later["workspace"], first["workspace"], "{later}");
}

/// The directories `cargo heli gc --dry-run --json` lists.
fn gc(fixture: &Fixture) -> Vec<Value> {
    let output = fixture
        .heli(["gc", "--dry-run", "--json"])
        .output()
        .expect("run heli gc --dry-run");
    assert_success(&output, "cargo heli gc --dry-run --json");
    let listing: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("not JSON ({error}): {}", text(&output.stdout));
    });
    assert_eq!(listing["schema"], 1, "{listing}");
    listing["dirs"]
        .as_array()
        .unwrap_or_else(|| panic!("no dirs: {listing}"))
        .clone()
}

/// The build directory `gc --dry-run` lists for `workspace`, if any.
fn listed_build_dir(fixture: &Fixture, workspace: &Path) -> Option<Value> {
    let workspace = canonical(workspace);
    gc(fixture).into_iter().find(|dir| {
        dir["kind"] == "build"
            && dir["workspace"]
                .as_str()
                .is_some_and(|path| std::fs::canonicalize(path).ok() == Some(workspace.clone()))
    })
}

/// A build directory is busy while cargo builds in it (its lock cannot be
/// taken), live once the build is over and its workspace exists, and an
/// orphan once its worktree is removed. `gc --dry-run` deletes nothing.
#[test]
fn a_build_directory_is_busy_then_live_then_orphan() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let main = fixture.workspace();
    init_repo(&main);
    set_up_repository(&fixture, &main);
    let linked = main.join(".claude").join("worktrees").join("linked");
    git(
        &main,
        &["worktree", "add", "--quiet", &linked.to_string_lossy()],
    );

    let gate = fixture.path("gate");
    let build = heli_in(&fixture, &linked, &["build", "--quiet", "-p", "gated"])
        .env("HELI_FIXTURE_GATE", &gate)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli build");
    let busy = wait_for("the build directory to be busy", || {
        listed_build_dir(&fixture, &linked).filter(|dir| dir["state"] == "busy")
    });
    std::fs::write(&gate, "").expect("open the gate");
    let output = build.wait_with_output().expect("wait for the build");
    assert_success(&output, "cargo heli build -p gated");

    let live = listed_build_dir(&fixture, &linked).expect("listed after the build");
    assert_eq!(live["state"], "live", "{live}");
    assert_eq!(live["path"], busy["path"], "{live}");
    let path = PathBuf::from(live["path"].as_str().expect("a path"));
    assert_eq!(
        canonical(path.parent().and_then(Path::parent).expect("a store")),
        canonical(&store)
    );
    assert_eq!(
        canonical(live["repository"].as_str().expect("a repository")),
        canonical(main.join(".git"))
    );

    git(
        &main,
        &["worktree", "remove", "--force", &linked.to_string_lossy()],
    );
    let listing = gc(&fixture);
    let orphans: Vec<&Value> = listing
        .iter()
        .filter(|dir| dir["path"] == live["path"])
        .collect();
    assert_eq!(orphans.len(), 1, "{orphans:?}");
    assert_eq!(orphans[0]["state"], "orphan", "{orphans:?}");
    assert!(path.is_dir(), "gc --dry-run deletes nothing");
    assert!(
        !listing.iter().any(|dir| dir["kind"] == "target"),
        "a target directory that is gone is not listed: {listing:?}"
    );

    // What a removed worktree leaves behind does not bring it back.
    let stray = linked.join("target").join("debug");
    std::fs::create_dir_all(&stray).expect("leave a target directory behind");
    std::fs::write(stray.join("stray.bin"), "x").expect("and a file in it");
    let listing = gc(&fixture);
    let states: Vec<(&Value, &Value)> = listing
        .iter()
        .map(|dir| (&dir["kind"], &dir["state"]))
        .collect();
    assert_eq!(
        states,
        [
            (&json!("build"), &json!("orphan")),
            (&json!("target"), &json!("orphan"))
        ],
        "{listing:?}"
    );

    // A build directory only a bare cargo built in has no manifest.
    let output = fixture
        .cargo(["build", "--quiet", "-p", "echo-args"])
        .current_dir(&main)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run cargo build");
    assert_success(&output, "cargo build");
    let unknown: Vec<Value> = gc(&fixture)
        .into_iter()
        .filter(|dir| dir["kind"] == "build" && dir["path"] != live["path"])
        .collect();
    assert_eq!(unknown.len(), 1, "{unknown:?}");
    assert_eq!(unknown[0]["state"], "unknown", "{unknown:?}");
    assert_eq!(unknown[0]["workspace"], Value::Null, "{unknown:?}");

    let output = fixture
        .heli(["gc", "--dry-run"])
        .output()
        .expect("run heli gc --dry-run");
    assert_success(&output, "cargo heli gc --dry-run");
    let stdout = text(&output.stdout);
    let shown = path.display().to_string();
    assert!(
        stdout.lines().any(|line| line.starts_with("build ")
            && line.contains(" orphan ")
            && line.contains(&shown)),
        "{stdout}"
    );
    let output = fixture.heli(["gc"]).output().expect("run heli gc");
    assert_eq!(
        output.status.code(),
        Some(2),
        "gc evicts nothing yet: {output:?}"
    );
    assert!(path.is_dir(), "gc deletes nothing");
}

/// The bytes in the files under `path`, never through a link.
fn du(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_dir() {
        std::fs::read_dir(path)
            .expect("read a directory")
            .map(|entry| du(&entry.expect("an entry").path()))
            .sum()
    } else if metadata.is_file() {
        metadata.len()
    } else {
        0
    }
}

/// The sizes a listed directory gives, by category.
fn categories(dir: &Value) -> [(&'static str, u64); 6] {
    let size = |name: &str| {
        dir["by_category"][name]
            .as_u64()
            .unwrap_or_else(|| panic!("no {name} in {dir}"))
    };
    [
        ("incremental", size("incremental")),
        ("deps", size("deps")),
        ("build_scripts", size("build_scripts")),
        ("fingerprints", size("fingerprints")),
        ("tool_caches", size("tool_caches")),
        ("other", size("other")),
    ]
}

/// Sizes by category of a build directory: what cargo writes in the
/// profile's `incremental/`, `deps/`, `build/` and `.fingerprint/`, and the
/// rest.
fn build_dir_sizes(dir: &Path) -> [(&'static str, u64); 6] {
    let debug = dir.join("debug");
    let [incremental, deps, build_scripts, fingerprints] =
        ["incremental", "deps", "build", ".fingerprint"].map(|name| du(&debug.join(name)));
    let other = du(dir) - incremental - deps - build_scripts - fingerprints;
    [
        ("incremental", incremental),
        ("deps", deps),
        ("build_scripts", build_scripts),
        ("fingerprints", fingerprints),
        ("tool_caches", 0),
        ("other", other),
    ]
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

/// `gc --dry-run` gives each directory's size, by category, as the files
/// in it add up: a build directory's intermediates by kind, also below a
/// target triple, and a `target/` directory's tool caches, which only the
/// configuration's patterns name. A link is never followed.
#[test]
fn sizes_by_category_match_the_files() {
    let fixture = Fixture::new("basic");
    configure_store(&fixture, "");
    let main = fixture.workspace();
    init_repo(&main);
    set_up_repository(&fixture, &main);
    let output = heli_in(
        &fixture,
        &main,
        &["build", "--quiet", "-p", "echo-args", "-p", "slow-a"],
    )
    .output()
    .expect("run heli build");
    assert_success(&output, "cargo heli build");
    let target = main.join("target");
    let trybuild = target.join("tests").join("trybuild").join("project");
    std::fs::create_dir_all(&trybuild).expect("create a tool cache");
    std::fs::write(trybuild.join("out.bin"), vec![0u8; 1000]).expect("fill it");
    let check = target.join("cli-template-check");
    std::fs::create_dir_all(&check).expect("create another tool cache");
    std::fs::write(check.join("out.bin"), vec![0u8; 300]).expect("fill it");
    let outside = fixture.path("outside");
    std::fs::create_dir_all(&outside).expect("create a directory outside");
    std::fs::write(outside.join("big.bin"), vec![0u8; 500]).expect("fill it");
    link_dir(&outside, &target.join("link"));

    let build = wait_for("the build directory", || listed_build_dir(&fixture, &main));
    let path = PathBuf::from(build["path"].as_str().expect("a path"));
    let expected = build_dir_sizes(&path);
    for (category, size) in expected {
        assert!(
            category == "tool_caches" || size > 0,
            "the build left nothing in {category}: {expected:?}"
        );
    }
    assert_eq!(categories(&build), expected, "{build}");
    assert_eq!(build["size"].as_u64(), Some(du(&path)), "{build}");

    // A target triple holds a `-`; a directory without one is no triple.
    let triple = path.join("x86_64-unknown-test").join("debug").join("deps");
    std::fs::create_dir_all(&triple).expect("create a triple's deps/");
    std::fs::write(triple.join("unit.bin"), vec![0u8; 700]).expect("fill it");
    let no_triple = path.join("notriple").join("debug").join("deps");
    std::fs::create_dir_all(&no_triple).expect("create another deps/");
    std::fs::write(no_triple.join("unit.bin"), vec![0u8; 50]).expect("fill it");
    let mut expected = build_dir_sizes(&path);
    expected[1].1 += 700;
    expected[5].1 -= 700;
    let build = listed_build_dir(&fixture, &main).expect("the build directory");
    assert_eq!(categories(&build), expected, "{build}");

    let listed = wait_for("the target directory", || listed_target(&fixture, &target));
    assert_eq!(listed["state"], "live", "{listed}");
    let total = du(&target);
    assert_eq!(
        listed["size"].as_u64(),
        Some(total),
        "links count nothing: {listed}"
    );
    let only_other = [
        ("incremental", 0),
        ("deps", 0),
        ("build_scripts", 0),
        ("fingerprints", 0),
        ("tool_caches", 0),
        ("other", total),
    ];
    assert_eq!(categories(&listed), only_other, "no pattern, no tool cache");

    // `*/incremental` would name a build directory's caches: patterns are
    // for `target/` directories only.
    configure_store(
        &fixture,
        "tool_caches = [\"tests/tr?build/project/*\", \"cli-*-check\", \"*/incremental\"]\n",
    );
    let listed = listed_target(&fixture, &target).expect("the target directory");
    let with_caches = [
        ("incremental", 0),
        ("deps", 0),
        ("build_scripts", 0),
        ("fingerprints", 0),
        ("tool_caches", 1300),
        ("other", total - 1300),
    ];
    assert_eq!(categories(&listed), with_caches, "{listed}");
    let build = listed_build_dir(&fixture, &main).expect("the build directory");
    assert_eq!(categories(&build), expected, "{build}");
}

/// What `status --json` says of `store`, once its figures are
/// `expected_used` bytes by `expected` categories.
fn wait_for_store(
    fixture: &Fixture,
    store: &Path,
    expected: impl Fn() -> (u64, [(&'static str, u64); 6]),
) -> Value {
    let store = canonical(store);
    let mut last = Value::Null;
    let found = (0..600).find_map(|_| {
        let status = fixture.status();
        let listed = status["disk"]["stores"].as_array().and_then(|stores| {
            stores.iter().find(|entry| {
                std::fs::canonicalize(entry["path"].as_str().unwrap_or_default()).ok()
                    == Some(store.clone())
            })
        });
        if let Some(listed) = listed {
            let (used, by_category) = expected();
            if listed["used"].as_u64() == Some(used) && categories(listed) == by_category {
                return Some(listed.clone());
            }
            last = listed.clone();
        }
        std::thread::sleep(Duration::from_millis(50));
        None
    });
    found.unwrap_or_else(|| panic!("the store's figures never matched; last: {last}"))
}

/// The sizes of every build directory in `store`, added up.
fn store_sizes(store: &Path) -> (u64, [(&'static str, u64); 6]) {
    let mut sums = [
        ("incremental", 0),
        ("deps", 0),
        ("build_scripts", 0),
        ("fingerprints", 0),
        ("tool_caches", 0),
        ("other", 0),
    ];
    for dir in build_dirs(store) {
        for (sum, (_, size)) in sums.iter_mut().zip(build_dir_sizes(&dir)) {
            sum.1 += size;
        }
    }
    (du(store), sums)
}

/// `status` shows the disk each store takes, by category, as the daemon
/// counts it after each run; and the registered `target/` directories.
#[test]
fn status_shows_disk_use_per_store() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let main = fixture.workspace();
    init_repo(&main);
    set_up_repository(&fixture, &main);
    let build = |package: &str| {
        let output = heli_in(&fixture, &main, &["build", "--quiet", "-p", package])
            .output()
            .expect("run heli build");
        assert_success(&output, "cargo heli build");
    };

    build("slow-a");
    let first = wait_for_store(&fixture, &store, || store_sizes(&store));
    assert_eq!(first["budget"], Value::Null, "{first}");
    build("echo-args");
    let second = wait_for_store(&fixture, &store, || store_sizes(&store));
    assert!(
        second["used"].as_u64() > first["used"].as_u64(),
        "a run's growth is counted: {first} then {second}"
    );

    let status = fixture.status();
    let target = canonical(main.join("target"));
    let targets = status["disk"]["targets"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let listed = targets
        .iter()
        .find(|entry| {
            std::fs::canonicalize(entry["path"].as_str().unwrap_or_default()).ok()
                == Some(target.clone())
        })
        .unwrap_or_else(|| panic!("no target directory: {status}"));
    assert_eq!(listed["used"].as_u64(), Some(du(&target)), "{listed}");

    let output = fixture.heli(["status"]).output().expect("run heli status");
    assert_success(&output, "cargo heli status");
    let stdout = text(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with("disk "))
        .unwrap_or_else(|| panic!("no disk line: {stdout}"));
    assert!(
        line.contains(&store.display().to_string())
            && line.contains("incremental")
            && line.contains(" KiB"),
        "{line}"
    );

    // A directory that is gone leaves the figures once the daemon looks
    // again, as the next run ends; that one builds into the harness's
    // target directory.
    std::fs::remove_dir_all(&target).expect("remove the target directory");
    let output = fixture
        .heli(["build", "--quiet", "-p", "echo-args"])
        .current_dir(&main)
        .output()
        .expect("run heli build");
    assert_success(&output, "cargo heli build");
    wait_for("the removed target directory to leave", || {
        let status = fixture.status();
        let targets = status["disk"]["targets"].as_array().cloned()?;
        let shown = |entry: &Value| PathBuf::from(entry["path"].as_str().unwrap_or_default());
        let main = canonical(&main);
        let gone = !targets.iter().any(|entry| {
            let path = shown(entry);
            path.file_name().is_some_and(|name| name == "target")
                && path
                    .parent()
                    .and_then(|dir| std::fs::canonicalize(dir).ok())
                    == Some(main.clone())
        });
        let harness = canonical(fixture.target_dir());
        let other = targets
            .iter()
            .any(|entry| std::fs::canonicalize(shown(entry)).ok() == Some(harness.clone()));
        (gone && other).then_some(())
    });
}

/// Each store's figures are its own build directories': two stores on one
/// volume, the first serving it, the second named by a workspace's own
/// configuration, each count only what is in them.
#[test]
fn status_counts_each_store_on_its_own() {
    let fixture = Fixture::new("basic");
    let first = fixture.path("first-store");
    let second = fixture.path("second-store");
    fixture.configure(&format!(
        "[store]\npaths = ['{}', '{}']\n",
        first.display(),
        second.display()
    ));
    build_with_build_dir(&fixture, "a", &first.join("{workspace-path-hash}"));
    build_with_build_dir(&fixture, "b", &second.join("{workspace-path-hash}"));
    for store in [&first, &second] {
        let listed = wait_for_store(&fixture, store, || store_sizes(store));
        assert!(listed["used"].as_u64() > Some(0), "{listed}");
    }
}

/// The store setup offers is the one on the worktree's volume: a store
/// configured first, on another volume, does not serve this one.
#[test]
fn setup_offers_the_store_on_the_worktrees_volume() {
    let fixture = Fixture::new("basic");
    // Only named, never written: `--print` writes nothing.
    let elsewhere = if cfg!(windows) {
        // Another drive this machine has, else one it does not have.
        let home_drive = fixture.path("").to_string_lossy().to_uppercase();
        let drives: Vec<char> = ('C'..='Z')
            .filter(|letter| !home_drive.starts_with(&format!("{letter}:")))
            .collect();
        let letter = drives
            .iter()
            .find(|letter| Path::new(&format!("{letter}:\\")).exists())
            .or_else(|| drives.last())
            .expect("a drive letter");
        PathBuf::from(format!("{letter}:\\heli-store-never-written"))
    } else {
        // `/dev` is a file system of its own on Linux and macOS.
        PathBuf::from("/dev/heli-store-never-written")
    };
    let store = fixture.path("store");
    fixture.configure(&format!(
        "[store]\npaths = ['{}', '{}']\n",
        elsewhere.display(),
        store.display()
    ));
    let output = fixture
        .setup(None, ["--print"])
        .output()
        .expect("run heli setup --print");
    assert_success(&output, "cargo heli setup --print");
    let stdout = text(&output.stdout);
    let value = format!("build-dir = {}", build_dir_value(&store));
    assert!(stdout.lines().any(|line| line == value), "{stdout}");
}

/// A `target/` directory several workspaces build into belongs to the last
/// one heli ran in: it lives as long as that one does.
#[test]
fn a_shared_target_directory_belongs_to_its_last_workspace() {
    let fixture = Fixture::new("basic");
    let first = fixture.copy_workspace("first");
    let second = fixture.copy_workspace("second");
    let shared = fixture.target_dir();
    for workspace in [&first, &second] {
        let output = fixture
            .heli(["build", "--quiet", "-p", "echo-args"])
            .current_dir(workspace)
            .output()
            .expect("run heli build");
        assert_success(&output, "cargo heli build");
        wait_for("the target directory to be the workspace's", || {
            listed_target(&fixture, &shared).filter(|dir| {
                dir["workspace"]
                    .as_str()
                    .is_some_and(|path| canonical(path) == canonical(workspace))
            })
        });
    }
    std::fs::remove_dir_all(&first).expect("remove the first workspace");
    let listed = listed_target(&fixture, &shared).expect("the shared target directory");
    assert_eq!(listed["state"], "live", "{listed}");
}

/// Only a run of cargo is asked where it builds: an `exec` of a program
/// named `cargo` is, and an `exec` of any other program, even one that
/// builds, is not.
#[test]
fn only_a_run_of_cargo_is_asked_about_its_workspace() {
    let fixture = Fixture::new("basic");
    let bin = fixture.path("bin");
    std::fs::create_dir_all(&bin).expect("create a bin directory");
    let not_cargo = bin.join(exe("not-cargo"));
    std::fs::copy(support::cargo_binary(), &not_cargo).expect("copy cargo");
    let first = fixture.copy_workspace("first");
    let second = fixture.copy_workspace("second");

    let mut command = heli_in(&fixture, &first, &["exec", "--"]);
    command
        .arg(&not_cargo)
        .args(["build", "--quiet", "-p", "echo-args"]);
    let output = command.output().expect("run heli exec");
    assert_success(&output, "cargo heli exec -- not-cargo build");
    assert!(first.join("target").is_dir(), "it built");

    let output = heli_in(
        &fixture,
        &second,
        &["exec", "--", "cargo", "build", "--quiet", "-p", "echo-args"],
    )
    .output()
    .expect("run heli exec");
    assert_success(&output, "cargo heli exec -- cargo build");
    wait_for("the second target directory", || {
        listed_target(&fixture, &second.join("target"))
    });
    assert!(
        listed_target(&fixture, &first.join("target")).is_none(),
        "not-cargo was not asked"
    );
}

/// The target directory `gc --dry-run` lists at `target`, if any.
fn listed_target(fixture: &Fixture, target: &Path) -> Option<Value> {
    let target = canonical(target);
    gc(fixture).into_iter().find(|dir| {
        dir["kind"] == "target"
            && std::fs::canonicalize(dir["path"].as_str().expect("a path")).ok()
                == Some(target.clone())
    })
}

/// Builds `echo-args` with heli in a copy of the workspace whose Cargo
/// configuration sets `build-dir` to `build_dir`, and returns the copy once
/// the daemon has learned its directories (its target directory is
/// registered, which comes last).
fn build_with_build_dir(fixture: &Fixture, name: &str, build_dir: &Path) -> PathBuf {
    let workspace = fixture.copy_workspace(name);
    let config = workspace.join(".cargo");
    std::fs::create_dir_all(&config).expect("create .cargo");
    let value = build_dir
        .to_str()
        .expect("a Unicode path")
        .replace('\\', "\\\\");
    std::fs::write(
        config.join("config.toml"),
        format!("[build]\nbuild-dir = \"{value}\"\n"),
    )
    .expect("write the configuration");
    build_in(fixture, &workspace);
    wait_for("the target directory", || {
        listed_target(fixture, &workspace.join("target"))
    });
    workspace
}

/// heli writes owner manifests only into build directories of a store it
/// manages, `<store>/<2 hex digits>/<hex digits>`: not into one elsewhere,
/// and not into another directory of a store. On Windows a store's path
/// may be written in another case.
#[test]
fn manifests_go_only_into_a_stores_build_directories() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let elsewhere = fixture.path("elsewhere");

    build_with_build_dir(&fixture, "a", &elsewhere.join("{workspace-path-hash}"));
    let dirs = build_dirs(&elsewhere);
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    assert!(owner(&dirs[0]).is_none(), "no manifest outside a store");

    let plain = store.join("ab").join("not-a-hash");
    build_with_build_dir(&fixture, "b", &plain);
    assert!(plain.join("debug").is_dir(), "cargo built there");
    assert!(
        owner(&plain).is_none(),
        "no manifest beside the store's layout"
    );
    // Nor is such a directory one of the store's build directories.
    let outer = store.join("zz").join("0123456789abcd").join("debug");
    std::fs::create_dir_all(&outer).expect("create a directory in the store");
    let listed: Vec<Value> = gc(&fixture)
        .into_iter()
        .filter(|dir| dir["kind"] == "build")
        .collect();
    assert_eq!(listed, Vec::<Value>::new(), "not build directories");

    if cfg!(windows) {
        let shouted = PathBuf::from(store.to_string_lossy().to_uppercase());
        let workspace = build_with_build_dir(&fixture, "c", &shouted.join("{workspace-path-hash}"));
        find_owner(&store, &workspace);
    }
}

/// Holds `path` so that nobody else can open it, until dropped: opened
/// without sharing on Windows, made unreadable elsewhere.
struct Unopenable {
    #[cfg(windows)]
    _file: std::fs::File,
    #[cfg(unix)]
    path: PathBuf,
}

impl Unopenable {
    fn new(path: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let file = std::fs::File::options()
                .read(true)
                .share_mode(0)
                .open(path)
                .expect("open the lock without sharing");
            Self { _file: file }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000))
                .expect("make the lock unreadable");
            Self {
                path: path.to_path_buf(),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for Unopenable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o644));
    }
}

/// Each of cargo's three lock files makes a directory busy while it is held,
/// in a profile directory and in a target triple's: heli tries them and
/// lets go. A lock heli cannot even open counts as held. A build directory
/// without a manifest is unknown otherwise.
#[test]
fn any_of_cargos_locks_held_makes_a_directory_busy() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let dir = store.join("ab").join("0123456789abcd");
    let state = || {
        let listing = gc(&fixture);
        assert_eq!(listing.len(), 1, "{listing:?}");
        listing[0]["state"].as_str().expect("a state").to_owned()
    };
    for place in ["debug", "x86_64-unknown-test/debug"] {
        let profile = dir.join(place);
        std::fs::create_dir_all(&profile).expect("create a profile directory");
        for name in [".cargo-build-lock", ".cargo-lock", ".cargo-artifact-lock"] {
            let lock = profile.join(name);
            let file = std::fs::File::create(&lock).expect("create a lock");
            assert_eq!(state(), "unknown", "{place}/{name} free");
            file.lock().expect("take the lock");
            assert_eq!(state(), "busy", "{place}/{name} held");
            file.unlock().expect("let go of the lock");
            drop(file);
            assert_eq!(state(), "unknown", "{place}/{name} free again");
            let unopenable = Unopenable::new(&lock);
            assert_eq!(state(), "busy", "{place}/{name} cannot be opened");
            drop(unopenable);
            std::fs::remove_file(&lock).expect("remove the lock");
        }
    }
}

/// The files setup offers are the ones cargo reads: run in a linked
/// worktree, the repository's is the main worktree's `.cargo/config.toml`;
/// the user's is in `CARGO_HOME`, and there the older `config` when only
/// that exists.
#[test]
fn setup_offers_the_configuration_cargo_reads() {
    let fixture = Fixture::new("basic");
    configure_store(&fixture, "");
    let main = fixture.workspace();
    init_repo(&main);
    let linked = main.join(".claude").join("worktrees").join("linked");
    git(
        &main,
        &["worktree", "add", "--quiet", &linked.to_string_lossy()],
    );
    let cargo_home = fixture.home().join("elsewhere");
    std::fs::create_dir_all(&cargo_home).expect("create the Cargo home");
    std::fs::write(cargo_home.join("config"), "[build]\njobs = 2\n")
        .expect("write the user's configuration");

    let mut setup = fixture.setup(None, ["--print"]);
    setup.current_dir(&linked).env("CARGO_HOME", &cargo_home);
    let output = setup.output().expect("run heli setup --print");
    assert_success(&output, "cargo heli setup --print");
    let stdout = text(&output.stdout);
    let offered = |what: &str| -> PathBuf {
        stdout
            .lines()
            .find_map(|line| {
                let path = line.strip_prefix(what)?.strip_suffix(" would become:")?;
                Some(PathBuf::from(path))
            })
            .unwrap_or_else(|| panic!("no offer of {what}: {stdout}"))
    };
    let repository = offered("Cargo configuration for this repository ");
    assert!(repository.ends_with(Path::new(".cargo").join("config.toml")));
    let dir = repository
        .parent()
        .and_then(Path::parent)
        .expect("a worktree");
    assert_eq!(canonical(dir), canonical(&main), "{stdout}");
    assert_eq!(
        offered("Cargo configuration for every project "),
        cargo_home.join("config")
    );
}

/// A store that is not an absolute path, or a tool-cache pattern that does
/// not stay below `target/`, makes the configuration unusable: `gc` says
/// so, and setup offers no `build-dir`.
#[test]
fn a_store_configuration_heli_cannot_use_is_refused() {
    let fixture = Fixture::new("basic");
    for (config, says) in [
        ("[store]\npaths = ['relative/store']\n", "store.paths"),
        (
            "[store]\ntool_caches = [\"../outside\"]\n",
            "store.tool_caches",
        ),
        ("[store]\ntool_caches = [\"\"]\n", "store.tool_caches"),
    ] {
        fixture.configure(config);
        let output = fixture
            .heli(["gc", "--dry-run"])
            .output()
            .expect("run heli gc --dry-run");
        assert_eq!(output.status.code(), Some(1), "{config}: {output:?}");
        let stderr = text(&output.stderr);
        assert!(stderr.contains(says), "{config}: {stderr}");

        let output = fixture
            .setup(None, ["--print"])
            .output()
            .expect("run heli setup --print");
        assert_success(&output, "cargo heli setup --print");
        let stdout = text(&output.stdout);
        assert!(
            stdout.contains("Build store: none offered") && stdout.contains(says),
            "{config}: {stdout}"
        );
        assert!(!stdout.contains("build-dir = "), "{stdout}");
    }
}

/// The owner manifest of `workspace`'s build directory in `store`.
fn find_owner(store: &Path, workspace: &Path) -> Value {
    let workspace = canonical(workspace);
    build_dirs(store)
        .iter()
        .filter_map(|dir| owner(dir))
        .find(|owner| canonical(owner["workspace"].as_str().expect("a path")) == workspace)
        .unwrap_or_else(|| panic!("no manifest for {}", workspace.display()))
}

/// A Cargo configuration whose `build-dir` an earlier setup wrote with
/// another store gets the new one in its place. One that sets `build-dir`
/// itself, or is not TOML, is left as it is, with the reason.
#[test]
fn setup_replaces_only_its_own_build_dir() {
    let fixture = Fixture::new("basic");
    let store = configure_store(&fixture, "");
    let cargo_home = fixture.home().join(".cargo");
    std::fs::create_dir_all(&cargo_home).expect("create the Cargo home");
    let user_config = cargo_home.join("config.toml");

    std::fs::write(
        &user_config,
        format!("[build]\n{MARKER}\nbuild-dir = \"an-old-store\"\njobs = 2\n"),
    )
    .expect("write the user's configuration");
    let output = answered(fixture.setup(None, Vec::<String>::new()), "y\n");
    assert_success(&output, "cargo heli setup");
    assert_eq!(
        std::fs::read_to_string(&user_config).expect("the user's configuration"),
        format!(
            "[build]\n{MARKER}\nbuild-dir = {}\njobs = 2\n",
            build_dir_value(&store)
        )
    );

    // heli's comment alone, without its line after it, is not heli's line.
    std::fs::write(&user_config, format!("[build]\n{MARKER}\njobs = 2\n"))
        .expect("write the user's configuration");
    let output = answered(fixture.setup(None, Vec::<String>::new()), "y\n");
    assert_success(&output, "cargo heli setup");
    assert_eq!(
        std::fs::read_to_string(&user_config).expect("the user's configuration"),
        format!(
            "[build]\n{MARKER}\nbuild-dir = {}\n{MARKER}\njobs = 2\n",
            build_dir_value(&store)
        )
    );

    let unsafe_edit = format!(
        "setup cannot add build.build-dir to it safely; add `build-dir = {}` under [build] by hand",
        build_dir_value(&store)
    );
    // heli's lines inside a string are the string's: taking them out would
    // change it, and here would leave no TOML at all.
    let in_a_string = format!("notes = \"\"\"\n{MARKER}\nbuild-dir = \"x\"\n\"\"\"\n");
    let in_a_value = format!("[build]\n{MARKER}\nbuild-dir = \"\"\"\nx\n\"\"\"\n");
    for (content, reason) in [
        (
            "[build]\nbuild-dir = \"mine\"\n",
            "it sets build.build-dir itself, to \"mine\"",
        ),
        ("[build]\nbuild-dir = 3\n", "it sets build.build-dir itself"),
        (
            "[build]\nbuild-dir = \"a \\\"b\\\" c\\\\d\\te\"\n",
            "it sets build.build-dir itself, to \"a \\\"b\\\" c\\\\d\\u0009e\"",
        ),
        ("[build\n", "it is not TOML"),
        (&in_a_string, &unsafe_edit),
        (&in_a_value, &unsafe_edit),
    ] {
        std::fs::write(&user_config, content).expect("write the user's configuration");
        let output = answered(fixture.setup(None, Vec::<String>::new()), "y\n");
        assert_success(&output, "cargo heli setup");
        let stdout = text(&output.stdout);
        let left = format!(
            "Cargo configuration for every project {} is left as it is: {reason}.",
            user_config.display()
        );
        assert!(
            stdout.lines().any(|line| line == left),
            "{content}: {stdout}"
        );
        assert!(!stdout.contains("[y/N]"), "{stdout}");
        assert_eq!(
            std::fs::read_to_string(&user_config).expect("the user's configuration"),
            content
        );
    }
}
