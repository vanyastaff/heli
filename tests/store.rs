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

use serde_json::Value;
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
    for path in [&repository_config, &user_config] {
        assert!(
            stdout.contains(&format!("Write {}? [y/N] ", path.display())),
            "setup offers {}: {stdout}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(&repository_config).expect("the repository's configuration"),
        fresh_config(&store)
    );
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
        "# the user's own\n[build]\njobs = 2\n\n[env]\nFOO = \"bar\"\n",
    )
    .expect("write the user's configuration");

    let output = answered(fixture.setup(None, Vec::<String>::new()), "y\n");
    assert_success(&output, "cargo heli setup");
    let stdout = text(&output.stdout);
    let none = format!(
        "Cargo configuration for this repository: none ({} is not in a git repository).",
        workspace.display()
    );
    assert!(stdout.lines().any(|line| line == none), "{stdout}");
    assert_eq!(stdout.matches("[y/N]").count(), 1, "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&user_config).expect("the user's configuration"),
        format!(
            "# the user's own\n[build]\n{MARKER}\nbuild-dir = {}\njobs = 2\n\n\
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
    let orphans: Vec<Value> = gc(&fixture)
        .into_iter()
        .filter(|dir| dir["path"] == live["path"])
        .collect();
    assert_eq!(orphans.len(), 1, "{orphans:?}");
    assert_eq!(orphans[0]["state"], "orphan", "{orphans:?}");
    assert!(path.is_dir(), "gc --dry-run deletes nothing");
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

    for (content, reason) in [
        (
            "[build]\nbuild-dir = \"mine\"\n",
            "it sets build.build-dir itself, to \"mine\"",
        ),
        ("[build\n", "it is not TOML"),
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
