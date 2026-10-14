//! Recycling (DESIGN.md §6.4): a worktree without a build directory gets
//! the most recently used orphan of its repository, renamed into place, and
//! the path scan invalidates every unit that compiled in a path of the
//! removed worktree or of the old build directory.
//!
//! Fixture `recycle` holds a repository, `repo/`, and two packages outside
//! it, `vendor/`, which its members depend on by path: they stand in for
//! third-party crates, the same files for every worktree. `plain-dep`
//! compiles in no path; `outdir-dep` compiles in its build script's
//! `OUT_DIR` and reads a file there at run time. In the repository,
//! `recycle-app` prints which source it was built from and what both give
//! it, and `manifest-dir`'s test reads its fixture file through
//! `env!("CARGO_MANIFEST_DIR")`.
//!
//! Each test makes `repo/` a git repository, and its worktrees beside it
//! (`<fixture>/ws/<name>`), so that the same relative path reaches
//! `vendor/` from each. Cargo's `build-dir` for all of them is set in
//! `<fixture>/ws/.cargo/config.toml`, to the store the fixture configures,
//! and their builds run without the harness's `CARGO_TARGET_DIR`. git runs
//! with no global or system configuration, so it runs no hook but the
//! fixture repository's own.

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{Fixture, assert_success};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

/// Whether `a` and `b` name the same existing place.
fn same_place(a: impl AsRef<Path>, b: impl AsRef<Path>) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether two spellings of a path that may no longer exist name the same
/// place: compared by their components, on Windows without case and with
/// either separator.
fn same_spelling(a: &str, b: &Path) -> bool {
    let spelled = |path: &str| -> Vec<String> {
        let path = if cfg!(windows) {
            path.replace('/', "\\").to_lowercase()
        } else {
            path.to_owned()
        };
        let separator = if cfg!(windows) { '\\' } else { '/' };
        path.trim_start_matches(r"\\?\")
            .split(separator)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    };
    spelled(a) == spelled(&b.to_string_lossy())
}

/// A fixture repository, its worktrees beside it, and the build store they
/// build into.
struct Repo {
    fixture: Fixture,
    store: PathBuf,
}

impl Repo {
    /// Fixture `recycle`, with `repo/` a git repository of one commit.
    fn new() -> Self {
        let fixture = Fixture::new("recycle");
        let store = fixture.path("store");
        fixture.configure(&format!("[store]\npaths = ['{}']\n", store.display()));
        let cargo = fixture.workspace().join(".cargo");
        std::fs::create_dir_all(&cargo).expect("create .cargo/");
        let build_dir = store.join("{workspace-path-hash}");
        let build_dir = build_dir.to_str().expect("a Unicode path");
        std::fs::write(
            cargo.join("config.toml"),
            format!(
                "[build]\nbuild-dir = \"{}\"\n",
                build_dir.replace('\\', "\\\\")
            ),
        )
        .expect("write the Cargo configuration");
        std::fs::write(fixture.path("gitconfig"), "").expect("write an empty git configuration");
        let repo = Self { fixture, store };
        let main = repo.main();
        repo.git(&main, &["init", "--quiet"]);
        repo.git(&main, &["add", "-A"]);
        repo.commit(&main, "fixture");
        repo
    }

    /// The repository's main worktree.
    fn main(&self) -> PathBuf {
        self.fixture.workspace().join("repo")
    }

    /// Where worktree `name` goes: beside the main worktree.
    fn worktree(&self, name: &str) -> PathBuf {
        self.fixture.workspace().join(name)
    }

    /// Points `command`'s git at the fixture's own configuration: none
    /// global, none system-wide, so no hook runs but the repository's own.
    fn isolate_git(&self, command: &mut Command) {
        command
            .env("GIT_CONFIG_GLOBAL", self.fixture.path("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }

    /// `git <args>` in `dir`, as the tests run git; panics unless it
    /// succeeds, and returns its output.
    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        // With the fixture's variables, so that a hook that runs heli finds
        // the fixture's daemon, configuration and store.
        let mut command = self.fixture.command("git", args.iter().copied());
        command.current_dir(dir).env_remove("CARGO_TARGET_DIR");
        self.isolate_git(&mut command);
        let output = command.output().expect("run git");
        assert_success(&output, &format!("git {}", args.join(" ")));
        output
    }

    /// Commits everything in `dir`.
    fn commit(&self, dir: &Path, message: &str) {
        self.git(
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
                message,
            ],
        );
    }

    /// Adds worktree `name`, on a branch of its own, and returns its path.
    fn add_worktree(&self, name: &str) -> PathBuf {
        let path = self.worktree(name);
        let spelled = path.to_string_lossy().into_owned();
        self.git(
            &self.main(),
            &["worktree", "add", "--quiet", "-b", name, &spelled],
        );
        path
    }

    /// Removes worktree `name`, whatever it holds.
    fn remove_worktree(&self, name: &str) {
        let spelled = self.worktree(name).to_string_lossy().into_owned();
        self.git(&self.main(), &["worktree", "remove", "--force", &spelled]);
    }

    /// `cargo heli <args>` in `dir`, building into its own `target/` and the
    /// store.
    fn heli(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = self.fixture.heli(args.iter().copied());
        command.current_dir(dir).env_remove("CARGO_TARGET_DIR");
        self.isolate_git(&mut command);
        command
    }

    /// Builds every target in `dir` with heli, and returns what cargo said
    /// of each unit, and the client's stderr.
    fn build(&self, dir: &Path) -> (Vec<Unit>, String) {
        let output = self
            .heli(
                dir,
                &[
                    "--output",
                    "full",
                    "build",
                    "--all-targets",
                    "--message-format=json",
                ],
            )
            .output()
            .expect("run heli build");
        assert_success(&output, &format!("cargo heli build in {}", dir.display()));
        (units(&output.stdout), text(&output.stderr))
    }

    /// The build directory whose owner manifest names `workspace`, once
    /// there is one.
    fn build_dir_of(&self, workspace: &Path) -> PathBuf {
        wait_for(
            &format!("the build directory of {}", workspace.display()),
            || {
                build_dirs(&self.store).into_iter().find(|dir| {
                    owner(dir).is_some_and(|owner| {
                        owner["workspace"]
                            .as_str()
                            .is_some_and(|path| same_place(path, workspace))
                    })
                })
            },
        )
    }

    /// The `recycle` events in the event log, oldest first.
    fn recycled(&self) -> Vec<Value> {
        self.fixture.events_named("recycle")
    }

    /// What the app built in `worktree` prints.
    fn run_app(&self, worktree: &Path) -> String {
        let app = worktree
            .join("target")
            .join("debug")
            .join(format!("recycle-app{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new(&app).output().expect("run the app");
        assert_success(&output, "the app");
        text(&output.stdout).trim_end().to_owned()
    }
}

/// What cargo said of one unit it built or found fresh.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Unit {
    /// The target's name, with ` (test)` for a test build of it.
    name: String,
    fresh: bool,
}

/// The units in cargo's JSON messages (`--message-format=json`), in order.
fn units(stdout: &[u8]) -> Vec<Unit> {
    let mut units: Vec<Unit> = text(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .map(|message| {
            let name = message["target"]["name"].as_str().expect("a target name");
            let test = message["profile"]["test"] == true;
            Unit {
                name: if test {
                    format!("{name} (test)")
                } else {
                    name.to_owned()
                },
                fresh: message["fresh"] == true,
            }
        })
        .collect();
    units.sort();
    units
}

/// The names of `units` that were `fresh`, or compiled.
fn names(units: &[Unit], fresh: bool) -> Vec<&str> {
    units
        .iter()
        .filter(|unit| unit.fresh == fresh)
        .map(|unit| unit.name.as_str())
        .collect()
}

/// Every unit of the fixture's workspace and its two dependencies, built
/// with `--all-targets`, sorted by name.
const ALL_UNITS: [&str; 8] = [
    "build-script-build",
    "fixture (test)",
    "manifest_dir",
    "manifest_dir (test)",
    "outdir_dep",
    "plain_dep",
    "recycle-app",
    "recycle-app (test)",
];

/// The owner manifest of build directory `dir`, if it has one.
fn owner(dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(dir.join(".heli").join("owner")).ok()?;
    serde_json::from_str(&text).ok()
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

/// Worktree a, built, is removed, and worktree b added: b's first build
/// gets a's build directory, renamed to b's place. The workspace members
/// compile, since b's sources are newer than every artifact; of the
/// packages outside the workspace, `plain-dep` stays fresh, and only
/// `outdir-dep`, which compiled in a's `OUT_DIR`, is rebuilt, invalidated
/// by the path scan, and reads its file at the new path. The event log
/// says what was recycled, from where, and how many units the scan
/// invalidated; so does a note to the run's client.
#[test]
fn a_new_worktree_builds_warm_in_a_removed_ones_build_directory() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    let (built, _) = repo.build(&a);
    assert_eq!(names(&built, false), ALL_UNITS, "a builds cold: {built:?}");
    let a_dir = repo.build_dir_of(&a);
    repo.remove_worktree("a");
    let b = repo.add_worktree("b");

    let (built, stderr) = repo.build(&b);
    assert_eq!(
        names(&built, true),
        ["build-script-build", "plain_dep"],
        "{built:?}"
    );
    assert_eq!(
        names(&built, false),
        [
            "fixture (test)",
            "manifest_dir",
            "manifest_dir (test)",
            "outdir_dep",
            "recycle-app",
            "recycle-app (test)",
        ],
        "{built:?}"
    );
    assert_eq!(
        repo.run_app(&b),
        "built from the commit; plain-dep; generated by outdir-dep"
    );

    let b_dir = repo.build_dir_of(&b);
    assert!(!a_dir.exists(), "a's build directory moved");
    assert_eq!(build_dirs(&repo.store), std::slice::from_ref(&b_dir));
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    let event = &recycled[0];
    assert_eq!(event["trigger"], "run", "{event}");
    assert_eq!(event["invalidated"], 2, "{event}");
    let path = |field: &str| {
        event[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field}: {event}"))
    };
    assert!(same_spelling(path("from"), &a_dir), "{event}");
    assert!(same_place(path("to"), &b_dir), "{event}");
    assert!(same_spelling(path("donor"), &a), "{event}");
    assert!(same_place(path("workspace"), &b), "{event}");
    let note = stderr
        .lines()
        .find(|line| line.starts_with("cargo-heli: note: recycled "))
        .unwrap_or_else(|| panic!("no note of the recycling: {stderr}"));
    assert!(
        note.ends_with("; the path scan invalidated 2 units"),
        "{note}"
    );
}

/// With no orphan in the store, a new worktree's first build is a cold
/// one: a live worktree's build directory is never taken, and stays its
/// own.
#[test]
fn without_an_orphan_a_new_worktree_builds_cold() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    let b = repo.add_worktree("b");

    let (built, stderr) = repo.build(&b);
    assert_eq!(names(&built, false), ALL_UNITS, "b builds cold: {built:?}");
    assert!(!stderr.contains("recycled"), "{stderr}");
    let b_dir = repo.build_dir_of(&b);
    assert_ne!(a_dir, b_dir);
    assert!(
        owner(&a_dir).is_some_and(|owner| owner["workspace"]
            .as_str()
            .is_some_and(|path| same_place(path, &a))),
        "a keeps its build directory"
    );
    assert_eq!(repo.recycled(), Vec::<Value>::new());
}

/// A member changed in the new worktree before its first build is
/// rebuilt from the new source: the binary never links the donor's code.
#[test]
fn a_member_changed_before_the_first_build_is_built_from_the_new_source() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    repo.build_dir_of(&a);
    repo.remove_worktree("a");
    let b = repo.add_worktree("b");
    let main = b.join("app").join("src").join("main.rs");
    let source = std::fs::read_to_string(&main).expect("read the app's source");
    std::fs::write(
        &main,
        source.replace("built from the commit", "built from b's change"),
    )
    .expect("change the app in b");

    let (built, _) = repo.build(&b);
    assert!(
        built
            .iter()
            .any(|unit| unit.name == "plain_dep" && unit.fresh),
        "b's build is warm: {built:?}"
    );
    assert_eq!(
        repo.run_app(&b),
        "built from b's change; plain-dep; generated by outdir-dep"
    );
}

/// Sets the modification time of every file under `dir`, but git's, to
/// `time`.
fn set_mtimes(dir: &Path, time: std::time::SystemTime) {
    for entry in std::fs::read_dir(dir).expect("read a directory") {
        let entry = entry.expect("an entry");
        if entry.file_name() == ".git" {
            continue;
        }
        let kind = entry.file_type().expect("its type");
        if kind.is_dir() {
            set_mtimes(&entry.path(), time);
        } else if kind.is_file() {
            std::fs::File::options()
                .write(true)
                .open(entry.path())
                .and_then(|file| file.set_modified(time))
                .expect("set a modification time");
        }
    }
}

/// `manifest-dir`'s test finds its fixture through the path compiled in
/// with `env!("CARGO_MANIFEST_DIR")`. After recycling it reads the new
/// worktree's copy, not the one the removed worktree left behind, even when
/// every source in the new worktree looks as old as the donor's (as precise
/// recycling will make them look): cargo would find the test fresh, and
/// only the path scan rebuilds it.
#[test]
fn a_test_reads_its_fixture_from_the_new_worktree() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    repo.build_dir_of(&a);
    repo.remove_worktree("a");
    // What a removed worktree can leave behind, without its `Cargo.toml`.
    let left = a.join("manifest-dir").join("fixtures");
    std::fs::create_dir_all(&left).expect("leave a's fixtures behind");
    std::fs::write(left.join("data.txt"), "a's copy\n").expect("with a's copy");
    let b = repo.add_worktree("b");
    let data = b.join("manifest-dir").join("fixtures").join("data.txt");
    std::fs::write(&data, "b's copy\n").expect("write b's copy");
    let long_ago = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    set_mtimes(&b, long_ago);

    let output = repo
        .heli(
            &b,
            &[
                "--output",
                "full",
                "test",
                "-p",
                "manifest-dir",
                "--test",
                "fixture",
                "--",
                "--nocapture",
            ],
        )
        .output()
        .expect("run heli test");
    assert_success(&output, "cargo heli test");
    let stdout = text(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "fixture: b's copy"),
        "{stdout}"
    );
    assert_eq!(repo.recycled().len(), 1);
}

/// `path` canonical, without Windows' `\\?\` before a drive: as heli
/// spells the paths it records.
fn plain(path: &Path) -> PathBuf {
    let canonical =
        std::fs::canonicalize(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let text = canonical.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => canonical,
    }
}

impl Repo {
    /// The repository's git common directory, as heli records it.
    fn repository(&self) -> PathBuf {
        plain(&self.main().join(".git"))
    }

    /// `cargo heli recycle` in `dir`, as git's `post-checkout` hook runs it.
    fn recycle_in(&self, dir: &Path) -> Output {
        let output = self
            .heli(dir, &["recycle"])
            .output()
            .expect("run heli recycle");
        assert_success(&output, "cargo heli recycle");
        output
    }

    /// A build directory made by hand in the store, at
    /// `<store>/<first 2 of hash>/<the rest>`, holding a file `marker` that
    /// says `label`, and the owner manifest `owner` if given.
    fn craft(&self, hash: &str, label: &str, owner: Option<Value>) -> PathBuf {
        let dir = self.store.join(&hash[..2]).join(&hash[2..]);
        std::fs::create_dir_all(dir.join("debug").join(".fingerprint"))
            .expect("create a build directory");
        std::fs::write(dir.join("marker"), label).expect("mark it");
        if let Some(owner) = owner {
            std::fs::create_dir_all(dir.join(".heli")).expect("create .heli/");
            std::fs::write(dir.join(".heli").join("owner"), owner.to_string())
                .expect("write the owner manifest");
        }
        dir
    }
}

/// An owner manifest for `workspace` in `repository`, last run at
/// `last_run`.
fn manifest(workspace: &Path, repository: Option<&Path>, last_run: u64) -> Value {
    serde_json::json!({
        "workspace": workspace,
        "repository": repository,
        "created_at": 1,
        "last_run": last_run,
    })
}

/// What the marker of build directory `dir` says.
fn marker(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("marker")).unwrap_or_default()
}

/// The build directory of a new worktree is the most recently used orphan
/// of its own repository. Never taken: an orphan of another repository or
/// of none (even a more recent one), a live build directory, a busy one
/// (one of cargo's locks in it held), one without a manifest, and what is
/// in the store's `.trash/`. The rest stay where they are.
#[test]
fn only_the_latest_free_orphan_of_the_same_repository_is_claimed() {
    let repo = Repo::new();
    let b = repo.add_worktree("b");
    let gone = |name: &str| repo.fixture.workspace().join(name);
    let ours = repo.repository();
    let elsewhere = repo.fixture.path("elsewhere.git");
    let dirs = [
        repo.craft(
            "a0000000000000",
            "another repository",
            Some(manifest(&gone("gone-1"), Some(&elsewhere), 900)),
        ),
        repo.craft(
            "a1000000000000",
            "no repository",
            Some(manifest(&gone("gone-2"), None, 800)),
        ),
        repo.craft(
            "a2000000000000",
            "live",
            Some(manifest(&repo.main(), Some(&ours), 700)),
        ),
        repo.craft(
            "a3000000000000",
            "busy",
            Some(manifest(&gone("gone-3"), Some(&ours), 600)),
        ),
        repo.craft("a4000000000000", "unknown", None),
        repo.craft(
            "a5000000000000",
            "older",
            Some(manifest(&gone("gone-4"), Some(&ours), 100)),
        ),
        repo.craft(
            "a6000000000000",
            "latest",
            Some(manifest(&gone("gone-5"), Some(&ours), 200)),
        ),
    ];
    let trash = repo.store.join(".trash").join("a7").join("000000000000");
    std::fs::create_dir_all(trash.join(".heli")).expect("create a directory in the trash");
    std::fs::write(
        trash.join(".heli").join("owner"),
        manifest(&gone("gone-6"), Some(&ours), 1000).to_string(),
    )
    .expect("write a manifest in the trash");
    let lock = dirs[3].join("debug").join(".cargo-build-lock");
    let held = std::fs::File::create(&lock).expect("create cargo's lock");
    held.lock().expect("hold cargo's lock");

    let output = repo.recycle_in(&b);
    let b_dir = repo.build_dir_of(&b);
    assert_eq!(marker(&b_dir), "latest", "{}", text(&output.stderr));
    for dir in &dirs[..6] {
        assert!(dir.join("marker").is_file(), "{} stays", marker(dir));
    }
    assert!(trash.is_dir(), "the trash is never taken from");
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["trigger"], "hook", "{recycled:?}");
    drop(held);
}

/// The content of rustc's dep-info for a unit that read each of `env`
/// (`NAME`, or `NAME=value` with the value as rustc escapes it).
fn dep_info(env: &[String]) -> String {
    let mut text = String::from("unit.d: src/lib.rs\n\nsrc/lib.rs:\n\n");
    for dep in env {
        text.push_str(&format!("# env-dep:{dep}\n"));
    }
    text
}

/// `value` escaped as rustc writes it in an `env-dep` line: `\` doubled.
fn escaped(value: impl AsRef<Path>) -> String {
    value.as_ref().to_string_lossy().replace('\\', "\\\\")
}

/// A unit made by hand in a build directory: its rustc dep-info and its
/// fingerprint directory.
struct CraftedUnit {
    /// The dep-info file, below the profile directory.
    dep_info: &'static str,
    /// `NAME=value` (the value unescaped) or `NAME`, for each variable it
    /// read.
    env: Vec<String>,
    /// Its fingerprint directory's name.
    fingerprint: &'static str,
    /// The dep-info file in the fingerprint directory, which names the
    /// target.
    dep: &'static str,
    /// Whether the scan deletes its fingerprint.
    invalidated: bool,
}

impl CraftedUnit {
    /// Writes it into `profile`.
    fn write(&self, profile: &Path) {
        let file = profile.join(self.dep_info);
        std::fs::create_dir_all(file.parent().expect("a directory")).expect("create its directory");
        let env: Vec<String> = self
            .env
            .iter()
            .map(|dep| match dep.split_once('=') {
                Some((name, value)) => format!("{name}={}", escaped(value)),
                None => dep.clone(),
            })
            .collect();
        std::fs::write(&file, dep_info(&env)).expect("write the dep-info");
        let fingerprint = profile.join(".fingerprint").join(self.fingerprint);
        std::fs::create_dir_all(&fingerprint).expect("create a fingerprint");
        std::fs::write(fingerprint.join(self.dep), "").expect("fill it");
    }
}

/// The path scan deletes the fingerprint of every unit whose rustc
/// dep-info has an `env-dep` value under the donor's workspace or the
/// donor's old path, read as rustc escapes it (`\` doubled) and as the
/// platform compares paths, whether the unit's dep-info is in `deps/`,
/// `examples/`, a build script's directory or a target triple's profile,
/// and whether its file names have a hash or not (on Windows MSVC, a path
/// package's executables). Units that read only other paths, a path that
/// merely starts with the same letters, or a variable without a value,
/// keep theirs. The event counts what it deleted.
#[test]
fn the_path_scan_invalidates_exactly_the_units_that_compiled_in_old_paths() {
    let repo = Repo::new();
    let b = repo.add_worktree("b");
    let ours = repo.repository();
    // On Unix a `\` is part of a name, and rustc escapes it.
    let donor = repo.fixture.workspace().join("gone\\donor");
    let dir = repo.craft(
        "b0000000000000",
        "donor",
        Some(manifest(&donor, Some(&ours), 1)),
    );
    let vendor = repo.fixture.workspace().join("vendor");
    let in_donor = |package: &str| {
        format!(
            "CARGO_MANIFEST_DIR={}",
            donor.join(package).to_string_lossy()
        )
    };
    let debug = dir.join("debug");
    let out_dir = debug
        .join("build")
        .join("pkg-b-9999999999999999")
        .join("out");
    let other_spelling = if cfg!(windows) {
        donor.to_string_lossy().replace('\\', "/").to_uppercase()
    } else {
        donor.to_string_lossy().replace('/', "//")
    };
    let list = std::env::join_paths([vendor.clone(), donor.join("bin")]).expect("a path list");
    let units = [
        CraftedUnit {
            dep_info: "deps/pkg_a-0123456789abcdef.d",
            env: vec![in_donor("pkg-a")],
            fingerprint: "pkg-a-0123456789abcdef",
            dep: "dep-lib-pkg_a",
            invalidated: true,
        },
        CraftedUnit {
            dep_info: "deps/pkg_b-1111111111111111.d",
            env: vec![format!("OUT_DIR={}", out_dir.to_string_lossy())],
            fingerprint: "pkg-b-1111111111111111",
            dep: "dep-lib-pkg_b",
            invalidated: true,
        },
        CraftedUnit {
            dep_info: "deps/pkg_c-2222222222222222.d",
            env: vec![
                format!(
                    "CARGO_MANIFEST_DIR={}",
                    vendor.join("pkg-c").to_string_lossy()
                ),
                "UNSET_VAR".to_owned(),
            ],
            fingerprint: "pkg-c-2222222222222222",
            dep: "dep-lib-pkg_c",
            invalidated: false,
        },
        CraftedUnit {
            dep_info: "deps/app_bin.d",
            env: vec![in_donor("app-bin")],
            fingerprint: "app-bin-3333333333333333",
            dep: "dep-bin-app-bin",
            invalidated: true,
        },
        CraftedUnit {
            dep_info: "deps/other_bin.d",
            env: vec![format!("CARGO_MANIFEST_DIR={}", vendor.to_string_lossy())],
            fingerprint: "other-bin-4444444444444444",
            dep: "dep-bin-other-bin",
            invalidated: false,
        },
        CraftedUnit {
            dep_info: "build/pkg-d-5555555555555555/build_script_build-5555555555555555.d",
            env: vec![in_donor("pkg-d")],
            fingerprint: "pkg-d-5555555555555555",
            dep: "dep-build-script-build-script-build",
            invalidated: true,
        },
        CraftedUnit {
            dep_info: "examples/demo-6666666666666666.d",
            env: vec![in_donor("pkg-e")],
            fingerprint: "pkg-e-6666666666666666",
            dep: "dep-example-demo",
            invalidated: true,
        },
        CraftedUnit {
            dep_info: "deps/pkg_g-7777777777777777.d",
            env: vec![format!("CARGO_MANIFEST_DIR={other_spelling}")],
            fingerprint: "pkg-g-7777777777777777",
            dep: "dep-lib-pkg_g",
            invalidated: true,
        },
        CraftedUnit {
            dep_info: "deps/pkg_h-8888888888888888.d",
            env: vec![format!("PATH={}", list.to_string_lossy())],
            fingerprint: "pkg-h-8888888888888888",
            dep: "dep-lib-pkg_h",
            invalidated: true,
        },
        CraftedUnit {
            dep_info: "deps/pkg_i-aaaaaaaaaaaaaaaa.d",
            env: vec![format!(
                "CARGO_MANIFEST_DIR={}",
                repo.fixture
                    .workspace()
                    .join("gone\\donor-not")
                    .to_string_lossy()
            )],
            fingerprint: "pkg-i-aaaaaaaaaaaaaaaa",
            dep: "dep-lib-pkg_i",
            invalidated: false,
        },
    ];
    for unit in &units {
        unit.write(&debug);
    }
    let in_triple = CraftedUnit {
        dep_info: "deps/pkg_f-bbbbbbbbbbbbbbbb.d",
        env: vec![in_donor("pkg-f")],
        fingerprint: "pkg-f-bbbbbbbbbbbbbbbb",
        dep: "dep-lib-pkg_f",
        invalidated: true,
    };
    in_triple.write(&dir.join("x86_64-unknown-test").join("debug"));

    let output = repo.recycle_in(&b);
    let b_dir = repo.build_dir_of(&b);
    assert_eq!(marker(&b_dir), "donor", "{}", text(&output.stderr));
    let kept = |profile: &Path, unit: &CraftedUnit| {
        profile.join(".fingerprint").join(unit.fingerprint).is_dir()
    };
    for unit in &units {
        assert_eq!(
            kept(&b_dir.join("debug"), unit),
            !unit.invalidated,
            "{}",
            unit.fingerprint
        );
    }
    assert!(!kept(
        &b_dir.join("x86_64-unknown-test").join("debug"),
        &in_triple
    ));
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["invalidated"], 8, "{recycled:?}");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("cargo-heli: recycled the build directory of ")
            && stderr.contains("the path scan invalidated 8 units"),
        "{stderr}"
    );
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

/// The files setup asked to write (`Write <path>? [y/N]`), in order.
fn asked(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| {
            let (path, _) = line.strip_prefix("Write ")?.split_once("? [y/N]")?;
            Some(PathBuf::from(path))
        })
        .collect()
}

impl Repo {
    /// `cargo heli setup` in the main worktree, answered with `answers`:
    /// its questions are this repository's Cargo configuration, the user's
    /// (in the fixture's temporary home), and the repository's
    /// `post-checkout` hook.
    fn setup(&self, answers: &str) -> Output {
        let mut command = self.fixture.setup(None, Vec::<String>::new());
        command.current_dir(self.main());
        self.isolate_git(&mut command);
        let output = answered(command, answers);
        assert_success(&output, "cargo heli setup");
        output
    }

    /// The repository's `post-checkout` hook.
    fn hook(&self) -> PathBuf {
        self.main().join(".git").join("hooks").join("post-checkout")
    }
}

/// `cargo heli setup` offers the repository a `post-checkout` hook, and
/// writes it on a yes, executable. git runs it as `git worktree add` checks
/// the new worktree out: the worktree gets the removed one's build
/// directory before anything builds in it, and git shows heli's line. A
/// checkout in a worktree that exists already recycles nothing, though an
/// orphan waits. Run again, setup finds the hook in place.
#[test]
fn setup_installs_a_hook_that_recycles_as_a_worktree_is_added() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    repo.remove_worktree("a");
    let gone = repo.fixture.workspace().join("gone");
    let spare = repo.craft(
        "c0000000000000",
        "spare",
        Some(manifest(&gone, Some(&repo.repository()), 1)),
    );

    let output = repo.setup("n\nn\ny\n");
    let stdout = text(&output.stdout);
    let questions = asked(&stdout);
    assert_eq!(questions.len(), 3, "{stdout}");
    assert!(same_place(&questions[2], repo.hook()), "{stdout}");
    let hook = std::fs::read_to_string(repo.hook()).expect("the hook is written");
    assert!(hook.starts_with("#!/bin/sh\n"), "{hook}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(repo.hook())
            .expect("the hook")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable: {mode:o}");
    }

    let b = repo.worktree("b");
    let output = repo.git(
        &repo.main(),
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "b",
            &b.to_string_lossy(),
        ],
    );
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("cargo-heli: recycled the build directory of ")
            && stderr.contains("the path scan invalidated 2 units"),
        "{stderr}"
    );
    let b_dir = repo.build_dir_of(&b);
    assert!(!a_dir.exists() && b_dir.join("debug").is_dir(), "a's moved");
    assert_eq!(marker(&spare), "spare", "the older orphan stays");
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["trigger"], "hook", "{recycled:?}");

    // Filled without a checkout, then checked out to a new branch: git
    // runs the hook, with the commit it came from.
    let c = repo.worktree("c");
    repo.git(
        &repo.main(),
        &[
            "worktree",
            "add",
            "--quiet",
            "--no-checkout",
            "--detach",
            &c.to_string_lossy(),
        ],
    );
    repo.git(&c, &["reset", "--quiet", "--hard"]);
    repo.git(&c, &["checkout", "--quiet", "-b", "c"]);
    assert_eq!(marker(&spare), "spare", "a checkout takes no orphan");
    assert_eq!(repo.recycled().len(), 1);
    repo.recycle_in(&c);
    assert_eq!(marker(&repo.build_dir_of(&c)), "spare", "c could take it");

    let stdout = text(&repo.setup("n\nn\n").stdout);
    assert_eq!(asked(&stdout).len(), 2, "{stdout}");
    assert!(
        stdout.contains(&format!(
            " {} already sets build directory recycling to this.",
            repo.hook().display()
        )),
        "{stdout}"
    );
}

/// A `post-checkout` hook that is not heli's is left as it is, with the
/// reason, and nothing about it is asked.
#[test]
fn setup_leaves_another_post_checkout_hook_alone() {
    let repo = Repo::new();
    std::fs::create_dir_all(repo.hook().parent().expect("hooks/")).expect("create hooks/");
    let theirs = "#!/bin/sh\necho theirs\n";
    std::fs::write(repo.hook(), theirs).expect("write their hook");

    let stdout = text(&repo.setup("n\nn\ny\n").stdout);
    assert_eq!(asked(&stdout).len(), 2, "{stdout}");
    assert!(
        stdout.contains(&format!(
            " {} is left as it is: it is not heli's;",
            repo.hook().display()
        )),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.hook()).expect("their hook"),
        theirs
    );
}

/// The daemon looks at the worktrees of every repository with an orphan
/// (`git worktree list`) as it starts, as runs end, and every minute. A
/// worktree added without the hook gets its build directory there, before
/// its first build, which is then warm; the main worktree, which is never
/// new, is left to its own first run.
#[test]
fn the_daemon_recycles_for_a_worktree_it_finds() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    repo.remove_worktree("a");
    let b = repo.add_worktree("b");

    repo.fixture.kill_daemon();
    let output = repo
        .heli(&repo.main(), &["ensure"])
        .output()
        .expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    let b_dir = repo.build_dir_of(&b);
    assert!(!a_dir.exists(), "a's moved");
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["trigger"], "look", "{recycled:?}");
    assert_eq!(recycled[0]["invalidated"], 2, "{recycled:?}");
    assert!(same_place(
        recycled[0]["to"].as_str().expect("a path"),
        &b_dir
    ));

    let (built, _) = repo.build(&b);
    assert_eq!(
        names(&built, true),
        ["build-script-build", "plain_dep"],
        "{built:?}"
    );
    assert_eq!(
        repo.run_app(&b),
        "built from the commit; plain-dep; generated by outdir-dep"
    );
}
