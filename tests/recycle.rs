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

/// Whether `a` and `b`, which no longer exist, named the same place: their
/// parents are the same existing place (resolved, as on macOS
/// `/private/var` for `/var`), and their names the same, on Windows
/// without case.
fn same_gone_place(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    let name = |path: &Path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if cfg!(windows) {
            name.to_lowercase()
        } else {
            name.into_owned()
        }
    };
    match (a.parent(), b.parent()) {
        (Some(parent_a), Some(parent_b)) => same_place(parent_a, parent_b) && name(a) == name(b),
        _ => false,
    }
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

    /// Removes worktree `removed` and adds worktree `added`, so that the
    /// first heli run in `added` recycles a build directory for it. The
    /// daemon's look, which recycles too, as it starts, as runs end and
    /// every minute (DESIGN.md §6.4), would race that run. So the daemon
    /// that built in `removed` is stopped, with any look it was about to
    /// make, and a new one has made its first look before `added` exists
    /// (each pass of its accountant looks first, then counts the stores
    /// that `status` shows); its next look is a minute away, or after the
    /// next run's end.
    fn replace_worktree(&self, removed: &str, added: &str) -> PathBuf {
        self.fixture.kill_daemon();
        self.remove_worktree(removed);
        let output = self
            .heli(&self.main(), &["ensure"])
            .output()
            .expect("run heli ensure");
        assert!(
            output.status.success(),
            "cargo heli ensure failed: {}\n--- the daemon's log\n{}",
            text(&output.stderr),
            self.fixture.daemon_log()
        );
        self.wait_for_first_look();
        self.add_worktree(added)
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
        let output = self.build_output(dir);
        (units(&output.stdout), text(&output.stderr))
    }

    /// Builds every target in `dir` with heli, and returns its output:
    /// cargo's JSON messages on stdout.
    fn build_output(&self, dir: &Path) -> Output {
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
        output
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
const ALL_UNITS: [&str; 13] = [
    "build-script-build",
    "extra (test)",
    "fixture (test)",
    "leaf",
    "leaf (test)",
    "manifest_dir",
    "manifest_dir (test)",
    "outdir_dep",
    "plain_dep",
    "recycle-app",
    "recycle-app (test)",
    "root",
    "root (test)",
];

/// The units of the fixture's workspace members, and `outdir-dep`, which
/// the path scan invalidates: what conservative recycling compiles.
const CONSERVATIVE_UNITS: [&str; 11] = [
    "extra (test)",
    "fixture (test)",
    "leaf",
    "leaf (test)",
    "manifest_dir",
    "manifest_dir (test)",
    "outdir_dep",
    "recycle-app",
    "recycle-app (test)",
    "root",
    "root (test)",
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
/// gets a's build directory, renamed to b's place. Without a source-state
/// record, recycling is conservative: the workspace members compile, since
/// b's sources are newer than every artifact; of the packages outside the
/// workspace, `plain-dep` stays fresh, and only `outdir-dep`, which
/// compiled in a's `OUT_DIR`, is rebuilt, invalidated by the path scan,
/// and reads its file at the new path. The event log says what was
/// recycled, from where, how, and how many units the scan invalidated; so
/// does a note to the run's client.
#[test]
fn a_new_worktree_builds_warm_in_a_removed_ones_build_directory() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    let (built, _) = repo.build(&a);
    assert_eq!(names(&built, false), ALL_UNITS, "a builds cold: {built:?}");
    let a_dir = repo.build_dir_of(&a);
    // Stopped first, the daemon cannot write the record again as it sees
    // the build's lock released.
    repo.fixture.kill_daemon();
    forget_record(&a_dir);
    let b = repo.replace_worktree("a", "b");

    let (built, stderr) = repo.build(&b);
    assert_eq!(
        names(&built, true),
        ["build-script-build", "plain_dep"],
        "{built:?}"
    );
    assert_eq!(names(&built, false), CONSERVATIVE_UNITS, "{built:?}");
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
    assert_eq!(event["mode"], "conservative", "{event}");
    let path = |field: &str| {
        event[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field}: {event}"))
    };
    assert!(same_gone_place(path("from"), &a_dir), "{event}");
    assert!(same_place(path("to"), &b_dir), "{event}");
    assert!(same_gone_place(path("donor"), &a), "{event}");
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

/// The source-state record heli writes in build directory `dir` after
/// each run.
fn record_file(dir: &Path) -> PathBuf {
    dir.join(".heli").join("sources")
}

/// Deletes the source-state record of build directory `dir`, which the
/// heli run that built there wrote.
fn forget_record(dir: &Path) {
    std::fs::remove_file(record_file(dir)).expect("delete the source-state record");
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
/// every source in the new worktree looks as old as the donor's (older
/// still than precise recycling makes them look): cargo would find the test
/// fresh, and only the path scan rebuilds it.
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
    // On one thread, as heli runs it when it gets one test slot, libtest
    // names the test before it runs, on the line the test prints on.
    assert!(
        stdout.lines().any(|line| {
            line.strip_prefix("test reads_its_fixture ... ")
                .unwrap_or(line)
                == "fixture: b's copy"
        }),
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
    /// says `label`, and the owner manifest `owner` if given. Its files are
    /// dated [`long_ago`], as a removed worktree's last build is older than
    /// a worktree made since: without a source-state record, recycling
    /// takes no donor built after a file of the new worktree was written.
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
        set_mtimes(&dir, long_ago());
        dir
    }
}

/// A time before any worktree of a test was made.
fn long_ago() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
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

/// Keeps the directory `dir` from being moved until dropped: on Windows a
/// file in it held open without sharing, elsewhere its parent read-only.
struct Unmovable {
    #[cfg(windows)]
    _file: std::fs::File,
    #[cfg(unix)]
    parent: PathBuf,
}

impl Unmovable {
    fn new(dir: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let file = std::fs::File::options()
                .read(true)
                .share_mode(0)
                .open(dir.join("marker"))
                .expect("hold a file in it");
            Self { _file: file }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let parent = dir.parent().expect("a parent").to_path_buf();
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555))
                .expect("make its parent read-only");
            Self { parent }
        }
    }
}

#[cfg(unix)]
impl Drop for Unmovable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.parent, std::fs::Permissions::from_mode(0o755));
    }
}

/// An orphan that cannot be moved now is passed over for the next one.
#[test]
fn an_orphan_that_cannot_be_moved_is_passed_over() {
    let repo = Repo::new();
    let b = repo.add_worktree("b");
    let ours = repo.repository();
    let gone = |name: &str| repo.fixture.workspace().join(name);
    // Not where b's build directory goes, which a read-only parent would
    // keep from being made.
    let b_dir = named_build_dir(&repo, &b);
    let b_outer = b_dir
        .parent()
        .and_then(Path::file_name)
        .expect("<store>/<2 hex digits>/");
    let hash = if b_outer == "d0" {
        "d2000000000000"
    } else {
        "d0000000000000"
    };
    let latest = repo.craft(
        hash,
        "latest",
        Some(manifest(&gone("gone-1"), Some(&ours), 200)),
    );
    repo.craft(
        "d1000000000000",
        "older",
        Some(manifest(&gone("gone-2"), Some(&ours), 100)),
    );
    let unmovable = Unmovable::new(&latest);

    let output = repo.recycle_in(&b);
    assert_eq!(
        marker(&repo.build_dir_of(&b)),
        "older",
        "{}",
        text(&output.stderr)
    );
    let c = repo.add_worktree("c");
    let output = repo.recycle_in(&c);
    drop(unmovable);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("cargo-heli: no build directory recycled: cannot move "),
        "with none left to try, it says why: {stderr}"
    );
    assert_eq!(marker(&latest), "latest");
    // Given up, it names its removed worktree again: an orphan still, for
    // a recycler that can move it.
    assert!(
        owner(&latest).is_some_and(|owner| owner["workspace"]
            .as_str()
            .is_some_and(|path| Path::new(path) == gone("gone-1"))),
        "{:?}",
        owner(&latest)
    );
}

/// The build directory cargo names for a worktree as heli would find it:
/// `build_directory` in `cargo metadata`, run in `dir`.
fn named_build_dir(repo: &Repo, dir: &Path) -> PathBuf {
    let output = repo
        .fixture
        .cargo(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run cargo metadata");
    assert_success(&output, "cargo metadata");
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    PathBuf::from(
        metadata["build_directory"]
            .as_str()
            .expect("a build directory"),
    )
}

/// A build directory that exists is never replaced, even an empty one.
#[test]
fn an_existing_build_directory_is_kept() {
    let repo = Repo::new();
    let b = repo.add_worktree("b");
    let gone = repo.fixture.workspace().join("gone");
    let orphan = repo.craft(
        "e0000000000000",
        "orphan",
        Some(manifest(&gone, Some(&repo.repository()), 1)),
    );
    let b_dir = named_build_dir(&repo, &b);
    std::fs::create_dir_all(&b_dir).expect("create b's build directory");

    repo.recycle_in(&b);
    assert_eq!(
        std::fs::read_dir(&b_dir)
            .expect("b's build directory")
            .count(),
        0,
        "still empty"
    );
    assert_eq!(marker(&orphan), "orphan");
    assert_eq!(repo.recycled(), Vec::<Value>::new());
}

/// The hook and the look recycle only for a workspace inside the worktree:
/// a worktree without a `Cargo.toml` at its top, inside another
/// workspace, gives that workspace no build directory.
#[test]
fn only_a_workspace_inside_the_worktree_gets_one() {
    let repo = Repo::new();
    // Another repository, without a workspace, inside one of repo's
    // packages: cargo, asked there, names repo's workspace.
    let other = repo.main().join("app").join("other");
    std::fs::create_dir_all(&other).expect("create another repository");
    std::fs::write(other.join("README"), "no workspace here\n").expect("fill it");
    repo.git(&other, &["init", "--quiet"]);
    repo.git(&other, &["add", "-A"]);
    repo.commit(&other, "another");
    let gone = repo.fixture.workspace().join("gone");
    let orphan = repo.craft(
        "f0000000000000",
        "orphan",
        Some(manifest(&gone, Some(&plain(&other.join(".git"))), 1)),
    );

    repo.recycle_in(&other);
    assert_eq!(marker(&orphan), "orphan");
    assert!(
        !named_build_dir(&repo, &repo.main()).exists(),
        "repo's workspace got nothing"
    );
    assert_eq!(repo.recycled(), Vec::<Value>::new());
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
    let mut units = Vec::from(units);
    if cfg!(windows) {
        // The verbatim spelling of the same path.
        units.push(CraftedUnit {
            dep_info: "deps/pkg_j-cccccccccccccccc.d",
            env: vec![format!(
                "CARGO_MANIFEST_DIR=\\\\?\\{}",
                donor.join("pkg-j").to_string_lossy()
            )],
            fingerprint: "pkg-j-cccccccccccccccc",
            dep: "dep-lib-pkg_j",
            invalidated: true,
        });
    }
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
    set_mtimes(&dir, long_ago());

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
    let invalidated = 1 + units.iter().filter(|unit| unit.invalidated).count();
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["invalidated"], invalidated, "{recycled:?}");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("cargo-heli: recycled the build directory of ")
            && stderr.contains(&format!("the path scan invalidated {invalidated} units")),
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

/// Whether setup said, in `stdout`, that the repository's `post-checkout`
/// hook `hook` is in the state `said` names (` is left as it is: …`).
/// Setup spells the hook's path as git does, which can differ from the
/// test's: on Windows without 8.3 short names, on macOS under `/private`.
fn hook_said(stdout: &str, hook: &Path, said: &str) -> bool {
    stdout.lines().any(|line| {
        line.strip_prefix("Git post-checkout hook for this repository ")
            .and_then(|rest| rest.split_once(said))
            .is_some_and(|(path, _)| same_place(path, hook))
    })
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
/// directory before anything builds in it, precisely with a's source-state
/// record, and git shows heli's line. A checkout in a worktree that exists
/// already recycles nothing, though an orphan waits. Run again, setup finds
/// the hook in place.
///
/// The hook needs no daemon. The one that built a is stopped before a is
/// removed: its look at the repository's worktrees, as the build ended,
/// could otherwise come late enough to recycle for b first, and the hook
/// would find b's build directory there.
#[test]
fn setup_installs_a_hook_that_recycles_as_a_worktree_is_added() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    repo.fixture.kill_daemon();
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
            && stderr.contains(", precisely (")
            && stderr.contains("the path scan invalidated 2 units"),
        "{stderr}\n{}",
        repo.store_diagnosis()
    );
    let b_dir = repo.build_dir_of(&b);
    assert!(!a_dir.exists() && b_dir.join("debug").is_dir(), "a's moved");
    assert_eq!(marker(&spare), "spare", "the older orphan stays");
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");
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
        hook_said(
            &stdout,
            &repo.hook(),
            " already sets build directory recycling to this."
        ),
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
        hook_said(
            &stdout,
            &repo.hook(),
            " is left as it is: it is not heli's;"
        ),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.hook()).expect("their hook"),
        theirs
    );
}

/// The hook names the heli that wrote it, wherever it is, a `'` and a
/// space in its path included. When that heli is gone, the hook still
/// never fails `git worktree add`; setup, run by another heli, replaces
/// the hook an earlier setup wrote.
#[test]
fn a_hook_never_fails_a_checkout_and_follows_heli() {
    let repo = Repo::new();
    let place = repo.fixture.path("it's elsewhere");
    std::fs::create_dir_all(&place).expect("create heli's other place");
    let moved = place.join(format!("moved-heli{}", std::env::consts::EXE_SUFFIX));
    support::copy_program(&support::heli_binary(), &moved);
    let gone = repo.fixture.workspace().join("gone");
    repo.craft(
        "c0000000000000",
        "orphan",
        Some(manifest(&gone, Some(&repo.repository()), 1)),
    );
    let template = repo.fixture.setup(None, Vec::<String>::new());
    let mut setup = Command::new(&moved);
    setup.arg("setup").current_dir(repo.main());
    for (name, value) in template.get_envs() {
        match value {
            Some(value) => setup.env(name, value),
            None => setup.env_remove(name),
        };
    }
    repo.isolate_git(&mut setup);
    let output = answered(setup, "n\nn\ny\n");
    assert_success(&output, "setup by a heli that moves away");
    let hook = std::fs::read_to_string(repo.hook()).expect("the hook");
    assert!(hook.contains("moved-heli"), "{hook}");
    let b = repo.add_worktree("b");
    assert_eq!(marker(&repo.build_dir_of(&b)), "orphan", "that heli ran");
    std::fs::remove_file(&moved).expect("heli moves away");

    let c = repo.add_worktree("c");
    assert!(c.join("Cargo.toml").is_file(), "c is checked out");

    let stdout = text(&repo.setup("n\nn\ny\n").stdout);
    assert_eq!(asked(&stdout).len(), 3, "{stdout}");
    let hook = std::fs::read_to_string(repo.hook()).expect("the hook");
    assert!(!hook.contains("moved-heli"), "replaced: {hook}");
}

/// The daemon looks at the worktrees of every repository with an orphan
/// (`git worktree list`) as it starts, as runs end, and every minute. A
/// worktree added without the hook gets its build directory there, before
/// its first build, which is then warm, and precise with a's record: only
/// what the path scan invalidated compiles. The main worktree, which is
/// never new, is left to its own first run.
///
/// The daemon that built a is stopped before a is removed: its look as the
/// build ended could otherwise come late enough to find b, and be stopped
/// halfway through recycling for it, leaving a's build directory claimed
/// for b and no recycle event.
#[test]
fn the_daemon_recycles_for_a_worktree_it_finds() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    repo.fixture.kill_daemon();
    repo.remove_worktree("a");
    let b = repo.add_worktree("b");

    let output = repo
        .heli(&repo.main(), &["ensure"])
        .output()
        .expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    let recycled = repo.wait_for_recycling();
    let b_dir = repo.build_dir_of(&b);
    assert!(!a_dir.exists(), "a's moved");
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["trigger"], "look", "{recycled:?}");
    assert_eq!(recycled[0]["invalidated"], 2, "{recycled:?}");
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");
    assert!(same_place(
        recycled[0]["to"].as_str().expect("a path"),
        &b_dir
    ));

    let (built, _) = repo.build(&b);
    assert_eq!(names(&built, false), SCANNED_UNITS, "{built:?}");
    assert_eq!(
        repo.run_app(&b),
        "built from the commit; plain-dep; generated by outdir-dep"
    );
}

/// What a test binary prints, run alone with its output shown: its
/// stdout, without the time `test result:` gives.
fn test_output(binary: &Path, dir: &Path) -> String {
    let output = Command::new(binary)
        .args(["--nocapture", "--test-threads=1"])
        .current_dir(dir)
        .output()
        .expect("run a test binary");
    text(&output.stdout)
        .lines()
        .map(|line| match line.split_once("; finished in ") {
            Some((result, _)) => result,
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The test binaries in cargo's JSON messages (`--message-format=json`),
/// named as [`units`] names them, each with what it prints when run in
/// `dir`.
fn test_outputs(stdout: &[u8], dir: &Path) -> Vec<(String, String)> {
    let mut outputs: Vec<(String, String)> = text(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact" && message["profile"]["test"] == true
        })
        .filter_map(|message| {
            let name = message["target"]["name"].as_str()?;
            let binary = message["executable"].as_str()?;
            Some((
                format!("{name} (test)"),
                test_output(Path::new(binary), dir),
            ))
        })
        .collect();
    outputs.sort();
    outputs
}

impl Repo {
    /// What the test binaries of a clean build of `worktree` print: built
    /// by cargo alone, in a build directory and a target directory of their
    /// own.
    fn clean_test_outputs(&self, worktree: &Path) -> Vec<(String, String)> {
        let clean = self.fixture.path("clean");
        let output = self
            .fixture
            .cargo(["build", "--all-targets", "--message-format=json"])
            .current_dir(worktree)
            .env("CARGO_TARGET_DIR", clean.join("target"))
            .env("CARGO_BUILD_BUILD_DIR", clean.join("build"))
            .output()
            .expect("run cargo build");
        assert_success(&output, "a clean cargo build");
        test_outputs(&output.stdout, worktree)
    }
}

/// A worktree's first build, which recycled a build directory.
struct Recycling {
    /// What cargo did of each unit.
    built: Vec<Unit>,
    /// The heli client's stderr.
    stderr: String,
    repo: Repo,
}

/// Worktree a, after `in_a` changed it, is built with heli, which leaves a
/// source-state record as the run ends, and is removed; worktree b is
/// added, `in_b` changes it, and heli builds it: b's first build recycles
/// a's build directory. b's test binaries must print what those of a clean
/// build of b print.
fn recycled_build(in_a: impl FnOnce(&Path), in_b: impl FnOnce(&Repo, &Path)) -> Recycling {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    in_a(&a);
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    assert!(record_file(&a_dir).is_file(), "a's run left a record");
    let b = repo.replace_worktree("a", "b");
    in_b(&repo, &b);

    let (built, stderr) = repo.build_like_a_clean_build(&b);
    Recycling {
        built,
        stderr,
        repo,
    }
}

impl Repo {
    /// Builds every target in `worktree` with heli, its first build there,
    /// which recycles a build directory; checks that its test binaries
    /// print what those of a clean build print, and returns what cargo did
    /// of each unit, and the client's stderr.
    fn build_like_a_clean_build(&self, worktree: &Path) -> (Vec<Unit>, String) {
        let output = self.build_output(worktree);
        assert_eq!(
            self.recycled().len(),
            1,
            "the first build recycled\n--- the daemon's log\n{}",
            self.fixture.daemon_log()
        );
        let recycled = test_outputs(&output.stdout, worktree);
        assert_eq!(recycled, self.clean_test_outputs(worktree));
        (units(&output.stdout), text(&output.stderr))
    }

    /// Plain cargo with `args` in `dir`, building into its own `target/`
    /// and the store; panics unless it succeeds.
    fn bare_cargo(&self, dir: &Path, args: &[&str]) {
        let output = self
            .fixture
            .cargo(args.iter().copied())
            .current_dir(dir)
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .expect("run cargo");
        assert_success(&output, &format!("cargo {}", args.join(" ")));
    }
}

/// The units precise recycling compiles in a worktree whose sources are
/// those of the removed one: the units the path scan invalidated
/// (`outdir-dep`, which compiled in its `OUT_DIR`, and `manifest-dir`'s
/// `fixture` test, its `CARGO_MANIFEST_DIR`), and what depends on them.
const SCANNED_UNITS: [&str; 4] = [
    "fixture (test)",
    "outdir_dep",
    "recycle-app",
    "recycle-app (test)",
];

/// Precise recycling: a's build leaves a source-state record, so b, whose
/// sources are a's, gets a's times on its files. Only the units the path
/// scan invalidated, and what depends on them, compile; every other member
/// of the workspace stays fresh. The recycle event says it was precise, and
/// how many files got their recorded times; so does the note to the run's
/// client.
#[test]
fn an_unchanged_worktree_recycles_precisely() {
    let Recycling {
        built,
        stderr,
        repo,
    } = recycled_build(|_| {}, |_, _| {});
    assert_eq!(names(&built, false), SCANNED_UNITS, "{built:?}");
    let recycled = repo.recycled();
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");
    let restored = recycled[0]["restored"].as_u64().expect("a count");
    assert!(restored > 0, "{recycled:?}");
    assert!(
        stderr.contains(&format!(
            ", a removed worktree, precisely ({restored} files got their recorded times); \
             the path scan invalidated 2 units"
        )),
        "{stderr}"
    );
}

/// Replaces `from` with `to` in file `path` of `worktree`.
fn change(worktree: &Path, path: &str, from: &str, to: &str) {
    let file = worktree.join(path);
    let source = std::fs::read_to_string(&file).expect("read a source");
    assert!(source.contains(from), "{path} says {from:?}");
    std::fs::write(&file, source.replace(from, to)).expect("change a source");
}

/// The units the path scan invalidated, what depends on them, and `more`,
/// sorted.
fn scanned_and(more: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = SCANNED_UNITS
        .iter()
        .chain(more)
        .map(|name| (*name).to_owned())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// With one crate changed in b, the leaf of the workspace's chain, precise
/// recycling compiles that crate's units, and those the path scan
/// invalidated with their dependents; its dependency `root`, and every
/// other unchanged member, stay fresh.
#[test]
fn a_changed_leaf_crate_compiles_alone() {
    let Recycling { built, .. } = recycled_build(
        |_| {},
        |_, b| {
            change(
                b,
                "leaf/src/lib.rs",
                "leaf from the commit",
                "leaf changed in b",
            )
        },
    );
    assert_eq!(
        names(&built, false),
        scanned_and(&["extra (test)", "leaf", "leaf (test)"]),
        "{built:?}"
    );
}

/// With the root of the workspace's chain changed in b, precise recycling
/// compiles it and what depends on it (`leaf`, and its tests), with the
/// units the path scan invalidated.
#[test]
fn a_changed_root_crate_compiles_with_its_dependents() {
    let Recycling { built, .. } = recycled_build(
        |_| {},
        |_, b| {
            change(
                b,
                "root/src/lib.rs",
                "root from the commit",
                "root changed in b",
            )
        },
    );
    assert_eq!(
        names(&built, false),
        scanned_and(&["extra (test)", "leaf", "leaf (test)", "root", "root (test)"]),
        "{built:?}"
    );
}

/// The record holds untracked files too. a builds two untracked tests of
/// `leaf`; b has one with the same content, which stays fresh, and the
/// other with different content, which compiles.
#[test]
fn untracked_files_are_recycled_by_content() {
    let test = |says: &str| {
        format!("#[test]\nfn prints() {{\n    println!(\"{says}: {{}}\", leaf::describe());\n}}\n")
    };
    let Recycling { built, .. } = recycled_build(
        |a| {
            write(a, "leaf/tests/added.rs", &test("added in a"));
            write(a, "leaf/tests/same.rs", &test("the same"));
        },
        |_, b| {
            write(b, "leaf/tests/added.rs", &test("added in b"));
            write(b, "leaf/tests/same.rs", &test("the same"));
        },
    );
    assert_eq!(
        names(&built, false),
        scanned_and(&["added (test)"]),
        "{built:?}"
    );
    assert!(
        built.contains(&Unit {
            name: "same (test)".to_owned(),
            fresh: true
        }),
        "{built:?}"
    );
}

/// A tracked file deleted in b, `leaf`'s `extra` test, is gone from its
/// build, as from a clean one; the rest recycles precisely.
#[test]
fn a_file_deleted_in_the_new_worktree_is_gone_from_its_build() {
    let Recycling { built, .. } = recycled_build(
        |_| {},
        |_, b| {
            std::fs::remove_file(b.join("leaf").join("tests").join("extra.rs"))
                .expect("delete leaf's extra test");
        },
    );
    assert_eq!(names(&built, false), SCANNED_UNITS, "{built:?}");
    assert!(
        !built.iter().any(|unit| unit.name == "extra (test)"),
        "{built:?}"
    );
}

/// `manifest-dir` is unchanged in b, so precise recycling keeps it fresh;
/// its `fixture` test, which finds its fixture file through the path
/// compiled in with `env!("CARGO_MANIFEST_DIR")`, is rebuilt all the same
/// by the path scan, and reads b's copy, not the one the removed worktree
/// left behind.
#[test]
fn an_unchanged_crate_reads_its_fixture_from_the_new_worktree() {
    let Recycling { built, .. } = recycled_build(
        |_| {},
        |repo, _| {
            write(
                &repo.worktree("a"),
                "manifest-dir/fixtures/data.txt",
                "a's copy\n",
            )
        },
    );
    let fresh = names(&built, true);
    assert!(
        fresh.contains(&"manifest_dir") && fresh.contains(&"manifest_dir (test)"),
        "{built:?}"
    );
    assert!(
        names(&built, false).contains(&"fixture (test)"),
        "{built:?}"
    );
}

/// Writes `content` to file `path` of `worktree`.
fn write(worktree: &Path, path: &str, content: &str) {
    let file = worktree.join(path);
    std::fs::create_dir_all(file.parent().expect("a directory")).expect("create its directory");
    std::fs::write(file, content).expect("write a file");
}

/// A build after the source-state record makes it stale: here plain cargo
/// builds a after `leaf` changed there. Recycling is then conservative:
/// every member compiles, and b's tests give what a clean build of b gives,
/// `leaf` from the commit. Taken at its word, the record would keep `leaf`
/// fresh, since b's source is the one it holds, and b's tests would run
/// a's change.
#[test]
fn a_build_after_the_record_makes_recycling_conservative() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    repo.build_dir_of(&a);
    // No daemon, which could write a record after this build.
    repo.fixture.kill_daemon();
    change(
        &a,
        "leaf/src/lib.rs",
        "leaf from the commit",
        "leaf changed in a",
    );
    repo.bare_cargo(&a, &["build", "--all-targets"]);
    // b's build would start a daemon whose first look races it.
    let b = repo.replace_worktree("a", "b");

    let (built, _) = repo.build_like_a_clean_build(&b);
    assert_eq!(names(&built, false), CONSERVATIVE_UNITS, "{built:?}");
    let recycled = repo.recycled();
    assert_eq!(recycled[0]["mode"], "conservative", "{recycled:?}");
}

/// Dates `to` every file under `dir` (but in heli's `.heli/`) last written
/// after `after`, in nanoseconds since 1970.
fn redate_newer(dir: &Path, after: u128, to: std::time::SystemTime) {
    for entry in std::fs::read_dir(dir).expect("read a directory").flatten() {
        let path = entry.path();
        let kind = entry.file_type().expect("a file type");
        if kind.is_dir() {
            if entry.file_name() != ".heli" {
                redate_newer(&path, after, to);
            }
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .expect("a modification time");
        let nanos = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        if kind.is_file() && nanos > after {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(to))
                .expect("date a file");
        }
    }
}

/// A build in the build directory right after the source-state record,
/// dated before the record, as the kernel's coarse clock (a tick behind the
/// clock heli reads) may date a write right after it, makes the record
/// stale all the same: a record counts only if it was taken a slack after
/// the time of each file there. Here plain cargo builds a after `leaf`
/// changed, and what it wrote is dated a millisecond before the record.
/// Recycling is then conservative; taken at its word, the record would keep
/// `leaf` fresh in b, with a's change.
#[test]
fn a_build_dated_just_before_the_record_makes_recycling_conservative() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    // No daemon, which could write a record after this build.
    repo.fixture.kill_daemon();
    let record = std::fs::read_to_string(record_file(&a_dir)).expect("a's record");
    let record: Value = serde_json::from_str(&record).expect("a record in JSON");
    let taken_at = record["taken_at"].as_u64().expect("its time");
    change(
        &a,
        "leaf/src/lib.rs",
        "leaf from the commit",
        "leaf changed in a",
    );
    repo.bare_cargo(&a, &["build", "--all-targets"]);
    let before = std::time::UNIX_EPOCH + Duration::from_nanos(taken_at - 1_000_000);
    redate_newer(&a_dir, u128::from(taken_at), before);
    // b's build would start a daemon whose first look races it.
    let b = repo.replace_worktree("a", "b");

    let (built, _) = repo.build_like_a_clean_build(&b);
    assert_eq!(names(&built, false), CONSERVATIVE_UNITS, "{built:?}");
    let recycled = repo.recycled();
    assert_eq!(recycled[0]["mode"], "conservative", "{recycled:?}");
}

/// What rustc writes in its incremental caches after the record does not
/// make the record stale: cargo decides nothing from them.
#[test]
fn rustcs_incremental_caches_leave_the_record_valid() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    let incremental = a_dir.join("debug").join("incremental");
    std::fs::create_dir_all(&incremental).expect("create incremental/");
    std::fs::write(incremental.join("written-later"), "").expect("write in incremental/");
    repo.remove_worktree("a");
    let b = repo.add_worktree("b");

    let (built, _) = repo.build_like_a_clean_build(&b);
    assert_eq!(names(&built, false), SCANNED_UNITS, "{built:?}");
}

/// A file of b that may have been written while heli staged b, its time not
/// before that, keeps its time, though it has the recorded content: here
/// `root`'s source, dated tomorrow. `root` compiles, with its dependents.
#[test]
fn a_file_written_while_heli_stages_the_worktree_keeps_its_time() {
    let Recycling { built, .. } = recycled_build(
        |_| {},
        |_, b| {
            let tomorrow = std::time::SystemTime::now() + Duration::from_secs(86_400);
            std::fs::File::options()
                .write(true)
                .open(b.join("root").join("src").join("lib.rs"))
                .and_then(|file| file.set_modified(tomorrow))
                .expect("date root's source tomorrow");
        },
    );
    assert_eq!(
        names(&built, false),
        scanned_and(&["extra (test)", "leaf", "leaf (test)", "root", "root (test)"]),
        "{built:?}"
    );
}

/// git's clean filter `later` (in Perl, which git for Windows has too), for
/// `leaf/src/lib.rs`: it passes the file through, and the first time it
/// runs after file `$marker` appears, it also writes the file again, right
/// after git read it, with leaf's text changed to b's, of the same size,
/// and dates it a whole second, one to two seconds back, as a file system
/// that keeps whole seconds (HFS+) or two (FAT) may date a write.
const CHANGE_AS_STAGED: &str = r#"
my ($file, $marker) = @ARGV;
binmode STDIN;
binmode STDOUT;
my $content = do { local $/; <STDIN> };
print $content;
if (unlink $marker) {
    (my $changed = $content) =~ s/leaf from the commit/leaf from b's change/ or die "no leaf";
    open my $out, '>', $file or die "$file: $!";
    binmode $out;
    print $out $changed;
    close $out or die "$file: $!";
    my $second = time() - 1;
    utime $second, $second, $file or die "$file: $!";
}
"#;

/// A file changed right after heli staged it, from the content the record
/// holds to another of the same size, keeps its time even when it is dated
/// before the staging, as a coarse file system or clock dates a write: the
/// record's time is only for a file written a slack before. Here git's
/// clean filter for b's `leaf/src/lib.rs` ([`CHANGE_AS_STAGED`]) changes it
/// as heli's staging reads it. Taken as written before the staging, the
/// file would get the recorded time, and b's build would link a's build of
/// the commit's `leaf`; it compiles b's change instead.
#[test]
fn a_file_changed_right_after_staging_keeps_its_time() {
    let repo = Repo::new();
    let script = repo.fixture.path("change-as-staged.pl");
    std::fs::write(&script, CHANGE_AS_STAGED).expect("write the filter");
    let marker = repo.fixture.path("change-once");
    let slashed = |path: &Path| path.to_string_lossy().replace('\\', "/");
    let main = repo.main();
    let filter = format!("perl '{}' %f '{}'", slashed(&script), slashed(&marker));
    repo.git(&main, &["config", "filter.later.clean", &filter]);
    let info = main.join(".git").join("info");
    std::fs::create_dir_all(&info).expect("create .git/info");
    std::fs::write(info.join("attributes"), "leaf/src/lib.rs filter=later\n")
        .expect("write the attributes");
    let a = repo.add_worktree("a");
    repo.build(&a);
    repo.build_dir_of(&a);
    let b = repo.replace_worktree("a", "b");
    // Dated otherwise than b's index says, so that git reads it again, and
    // runs the filter, as heli stages b.
    let lib = b.join("leaf").join("src").join("lib.rs");
    std::fs::File::options()
        .write(true)
        .open(&lib)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .expect("date b's leaf now");
    std::fs::write(&marker, "").expect("arm the filter");

    let output = repo.build_output(&b);
    assert!(
        !marker.exists(),
        "the filter changed b's leaf as heli staged it"
    );
    assert_eq!(
        leaf_says(&output.stdout, &b),
        "leaf: leaf from b's change over root from the commit",
        "{}",
        repo.diagnosis(&lib)
    );
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");
}

/// git's blob id is of the content after its filters, so a file whose line
/// endings alone differ has the recorded blob; its size tells them apart,
/// and it keeps its time. Here `*.txt` is text, checked out with line
/// feeds; a's `root/note.txt`, which `root` compiles in byte for byte, has
/// carriage returns too. In b, `root` compiles, and its test prints b's
/// note, as a clean build's does.
#[test]
fn a_file_with_other_line_endings_keeps_its_time() {
    let repo = Repo::new();
    let main = repo.main();
    write(&main, ".gitattributes", "*.txt text eol=lf\n");
    write(&main, "root/note.txt", "a note from the commit\n");
    repo.git(&main, &["add", "-A"]);
    repo.commit(&main, "a note with line feeds");
    let a = repo.add_worktree("a");
    write(&a, "root/note.txt", "a note from the commit\r\n");
    repo.build(&a);
    repo.build_dir_of(&a);
    repo.remove_worktree("a");
    let b = repo.add_worktree("b");

    let (built, _) = repo.build_like_a_clean_build(&b);
    assert_eq!(
        names(&built, false),
        scanned_and(&["extra (test)", "leaf", "leaf (test)", "root", "root (test)"]),
        "{built:?}"
    );
}

impl Repo {
    /// Waits until the daemon's accountant has looked at the store once:
    /// `status` then shows its stores.
    fn wait_for_first_look(&self) {
        self.fixture
            .wait_for_status("the daemon's first look", |status| {
                status["disk"]["stores"]
                    .as_array()
                    .is_some_and(|stores| !stores.is_empty())
            });
    }
}

/// The daemon writes a source-state record when it sees one of cargo's
/// locks in a build directory released: never while one is held, and not
/// for a build it did not see.
///
/// After a heli run, plain cargo builds a with `leaf` changed, which makes
/// the run's record stale, and the daemon, not running then, does not see
/// it. While a build holds the lock, a heli run in a that takes no lock
/// (`cargo metadata`) ends without writing a record. Once the daemon, which
/// saw the lock held, sees it released, it writes one, and b's first build
/// uses it: `leaf` compiles, since b has the commit's and the record a's
/// change, but `root` and the other members stay fresh.
#[test]
fn the_daemon_writes_a_record_when_a_build_lock_is_released() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    repo.fixture.kill_daemon();
    change(
        &a,
        "leaf/src/lib.rs",
        "leaf from the commit",
        "leaf changed in a",
    );
    repo.bare_cargo(&a, &["build", "--all-targets"]);
    let record = || std::fs::read_to_string(record_file(&a_dir)).expect("a's record");
    let stale = record();
    let ensure = repo
        .heli(&repo.main(), &["ensure"])
        .output()
        .expect("run heli ensure");
    assert_success(&ensure, "cargo heli ensure");
    repo.wait_for_first_look();
    assert_eq!(record(), stale, "no record of a build the daemon missed");
    repo.fixture.kill_daemon();

    let lock = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(a_dir.join("debug").join(".cargo-build-lock"))
        .expect("open cargo's lock");
    lock.lock().expect("hold cargo's lock");
    let metadata = ["metadata", "--format-version", "1", "--no-deps"];
    let output = repo
        .heli(&a, &metadata)
        .output()
        .expect("run heli metadata");
    assert_success(&output, "cargo heli metadata");
    assert_eq!(record(), stale, "no record while the lock is held");
    repo.wait_for_first_look();
    drop(lock);
    // A run that ends wakes the daemon's accountant.
    let output = repo
        .heli(&repo.main(), &metadata)
        .output()
        .expect("run heli metadata");
    assert_success(&output, "cargo heli metadata");
    wait_for("a record of a", || {
        let record = std::fs::read_to_string(record_file(&a_dir)).ok()?;
        (record != stale).then_some(())
    });
    repo.remove_worktree("a");
    let b = repo.add_worktree("b");

    let (built, _) = repo.build_like_a_clean_build(&b);
    assert_eq!(
        names(&built, false),
        scanned_and(&["extra (test)", "leaf", "leaf (test)"]),
        "{built:?}"
    );
}

/// A lock released over a record that still counts, nothing built since
/// it was taken, gets no new one: the worktree is left alone, as after a
/// heli run, whose end wrote the record already.
#[test]
fn a_record_that_counts_is_not_written_again() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let a_dir = repo.build_dir_of(&a);
    let record = || std::fs::read_to_string(record_file(&a_dir)).expect("a's record");
    let counting = record();
    repo.fixture.kill_daemon();
    let lock = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(a_dir.join("debug").join(".cargo-build-lock"))
        .expect("open cargo's lock");
    lock.lock().expect("hold cargo's lock");
    let ensure = repo
        .heli(&repo.main(), &["ensure"])
        .output()
        .expect("run heli ensure");
    assert_success(&ensure, "cargo heli ensure");
    repo.wait_for_first_look();
    drop(lock);
    let used = |status: &Value| status["disk"]["stores"][0]["used"].as_u64();
    let before = used(&repo.fixture.status());

    // A run in the main worktree ends, and the accountant looks again: it
    // counts what the run left in its build directory once it is done
    // with the locks.
    let metadata = ["metadata", "--format-version", "1", "--no-deps"];
    let output = repo
        .heli(&repo.main(), &metadata)
        .output()
        .expect("run heli metadata");
    assert_success(&output, "cargo heli metadata");
    repo.fixture
        .wait_for_status("the look after the run", |status| used(status) != before);
    assert_eq!(record(), counting);
}

impl Repo {
    /// Builds worktree `a`, changes its `leaf` crate (which nothing the path
    /// scan invalidates depends on) and builds it again, so that its last
    /// build is newer than every file of a worktree made before it; then
    /// stops the daemon (it cannot write a's record again), deletes a's
    /// source-state record unless `keep_record`, and removes `a`. Returns
    /// a's build directory, an orphan now.
    fn build_changed_and_remove(&self, a: &Path, keep_record: bool) -> PathBuf {
        self.build(a);
        change(
            a,
            "leaf/src/lib.rs",
            "leaf from the commit",
            "leaf from a's change",
        );
        let output = self.build_output(a);
        assert_eq!(
            leaf_says(&output.stdout, a),
            "leaf: leaf from a's change over root from the commit"
        );
        let a_dir = self.build_dir_of(a);
        self.fixture.kill_daemon();
        if !keep_record {
            forget_record(&a_dir);
        }
        self.remove_worktree("a");
        a_dir
    }
}

/// What `leaf`'s test prints, of a build whose JSON messages are `stdout`,
/// run in `dir`.
fn leaf_says(stdout: &[u8], dir: &Path) -> String {
    let outputs = test_outputs(stdout, dir);
    let (_, output) = outputs
        .iter()
        .find(|(name, _)| name == "leaf (test)")
        .unwrap_or_else(|| panic!("no test binary of leaf: {outputs:?}"));
    output
        .lines()
        .find_map(|line| line.find("leaf: ").map(|at| line[at..].to_owned()))
        .unwrap_or_else(|| panic!("leaf's test printed nothing: {output}"))
}

/// What `leaf`'s test prints when built from the commit.
const LEAF_FROM_THE_COMMIT: &str = "leaf: leaf from the commit over root from the commit";

/// A time well before any build of a test.
fn an_hour_ago() -> std::time::SystemTime {
    std::time::SystemTime::now() - Duration::from_secs(3600)
}

/// The main worktree, whose files are older than a's last build, recycles
/// a's build directory precisely at its first run: `leaf`, which a's
/// record holds with a's change, is rebuilt from the main worktree's own
/// source, and never links a's code; the rest stays warm.
#[test]
fn a_worktree_older_than_the_donors_build_recycles_precisely_without_its_code() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build_changed_and_remove(&a, true);
    let main = repo.main();
    set_mtimes(&main, an_hour_ago());

    let output = repo.build_output(&main);
    assert_eq!(leaf_says(&output.stdout, &main), LEAF_FROM_THE_COMMIT);
    let built = units(&output.stdout);
    assert!(
        built.iter().any(|unit| unit.name == "root" && unit.fresh),
        "the build is warm: {built:?}"
    );
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["trigger"], "run", "{recycled:?}");
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");
}

/// Without a record of the donor, a worktree whose files are not all newer
/// than the donor's last build does not recycle at its first run: cargo
/// would find a's units fresh. It builds cold, from its own sources, and
/// the orphan stays for another.
#[test]
fn a_worktree_older_than_the_donors_build_does_not_recycle_conservatively() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    let a_dir = repo.build_changed_and_remove(&a, false);
    let main = repo.main();
    set_mtimes(&main, an_hour_ago());

    let output = repo.build_output(&main);
    assert_eq!(leaf_says(&output.stdout, &main), LEAF_FROM_THE_COMMIT);
    let built = units(&output.stdout);
    assert_eq!(names(&built, false), ALL_UNITS, "cold: {built:?}");
    assert_eq!(repo.recycled(), Vec::<Value>::new());
    assert!(a_dir.is_dir(), "the orphan stays");
}

/// The daemon's look recycles precisely for a worktree made before a's
/// last build: `leaf` is rebuilt from the worktree's own source at its
/// first build.
#[test]
fn the_look_recycles_precisely_for_an_older_worktree_without_the_donors_code() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    let b = repo.add_worktree("b");
    let a_dir = repo.build_changed_and_remove(&a, true);
    let lib = b.join("leaf").join("src").join("lib.rs");
    let mut trace = format!(
        "a's build directory {}\nbefore set_mtimes {:?}, b's lib.rs {}\n",
        a_dir.display(),
        std::time::SystemTime::now(),
        mtime_of(&lib)
    );
    set_mtimes(&b, an_hour_ago());
    trace.push_str(&format!(
        "after set_mtimes {:?}, b's lib.rs {}\n",
        std::time::SystemTime::now(),
        mtime_of(&lib)
    ));
    let ensure = repo
        .heli(&repo.main(), &["ensure"])
        .output()
        .expect("run heli ensure");
    assert_success(&ensure, "cargo heli ensure");
    let recycled = repo.wait_for_recycling();
    assert_eq!(recycled[0]["trigger"], "look", "{recycled:?}");
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");
    let b_dir = repo.build_dir_of(&b);
    trace.push_str(&format!(
        "after the event {:?}, b's lib.rs {}\n{}",
        std::time::SystemTime::now(),
        mtime_of(&lib),
        times_in(&b_dir)
    ));

    let output = repo.build_output(&b);
    assert_eq!(
        leaf_says(&output.stdout, &b),
        LEAF_FROM_THE_COMMIT,
        "{trace}\nafter the build: units {:?}\n{}",
        units(&output.stdout),
        repo.diagnosis(&lib)
    );
}

/// `path`'s modification time, in nanoseconds since 1970, or why not.
fn mtime_of(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(|modified| {
            modified.duration_since(std::time::UNIX_EPOCH).map_or_else(
                |_| "before 1970".to_owned(),
                |since| since.as_nanos().to_string(),
            )
        })
        .unwrap_or_else(|error| error.to_string())
}

/// What decides freshness in build directory `dir`, for a failing test:
/// the newest file outside `.heli/` and `incremental/`, each of `leaf`'s
/// fingerprint files with its time, and what the source-state record holds
/// for `leaf/src/lib.rs`.
fn times_in(dir: &Path) -> String {
    fn walk(dir: &Path, top: bool, newest: &mut (u128, String)) {
        let profile = dir.join(".fingerprint").is_dir();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !(top && name == ".heli") && !(profile && name == "incremental") {
                    walk(&entry.path(), false, newest);
                }
            } else if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) {
                let ns = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_nanos());
                if ns > newest.0 {
                    *newest = (ns, entry.path().display().to_string());
                }
            }
        }
    }
    let mut newest = (0, String::new());
    walk(dir, true, &mut newest);
    let mut text = format!("newest {} {}\n", newest.0, newest.1);
    let fingerprints = dir.join("debug").join(".fingerprint");
    for unit in std::fs::read_dir(&fingerprints)
        .into_iter()
        .flatten()
        .flatten()
    {
        if !unit.file_name().to_string_lossy().starts_with("leaf-") {
            continue;
        }
        for file in std::fs::read_dir(unit.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            text.push_str(&format!(
                "  {} {}\n",
                file.path().display(),
                mtime_of(&file.path())
            ));
        }
    }
    let record = std::fs::read_to_string(record_file(dir)).unwrap_or_default();
    let record: Value = serde_json::from_str(&record).unwrap_or_default();
    text.push_str(&format!(
        "record taken_at {} leaf/src/lib.rs {}\n",
        record["taken_at"], record["files"]["leaf/src/lib.rs"]
    ));
    text
}

impl Repo {
    /// What a failing recycling test shows: the modification time of
    /// `file`, the recycle events and the daemon's log.
    fn diagnosis(&self, file: &Path) -> String {
        let modified = std::fs::metadata(file)
            .and_then(|metadata| metadata.modified())
            .map(|modified| format!("{modified:?}"))
            .unwrap_or_else(|error| error.to_string());
        format!(
            "{} modified {modified} (now {:?})\n--- recycle events\n{:?}\n--- the daemon's log\n{}",
            file.display(),
            std::time::SystemTime::now(),
            self.recycled(),
            self.fixture.daemon_log()
        )
    }

    /// What a failing recycling test shows of the store: each build
    /// directory with its owner manifest, the recycle events and the
    /// daemon's log.
    fn store_diagnosis(&self) -> String {
        let mut text = String::from("--- the store's build directories\n");
        for dir in build_dirs(&self.store) {
            let owner =
                owner(&dir).map_or_else(|| "no owner".to_owned(), |owner| owner.to_string());
            text.push_str(&format!("{} {owner}\n", dir.display()));
        }
        format!(
            "{text}--- recycle events\n{:?}\n--- the daemon's log\n{}",
            self.recycled(),
            self.fixture.daemon_log()
        )
    }

    /// The recycle events, once there is one; panics after 60 s with what
    /// the store holds.
    fn wait_for_recycling(&self) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let recycled = self.recycled();
            if !recycled.is_empty() {
                return recycled;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the recycle event\n{}",
                self.store_diagnosis()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Without a record of the donor, the look does not recycle for a worktree
/// made before a's last build; its first build is cold, from its own
/// sources.
#[test]
fn the_look_does_not_recycle_conservatively_for_an_older_worktree() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    let b = repo.add_worktree("b");
    let a_dir = repo.build_changed_and_remove(&a, false);
    set_mtimes(&b, an_hour_ago());
    let ensure = repo
        .heli(&repo.main(), &["ensure"])
        .output()
        .expect("run heli ensure");
    assert_success(&ensure, "cargo heli ensure");
    repo.wait_for_first_look();
    assert_eq!(repo.recycled(), Vec::<Value>::new());

    let output = repo.build_output(&b);
    assert_eq!(leaf_says(&output.stdout, &b), LEAF_FROM_THE_COMMIT);
    let built = units(&output.stdout);
    assert_eq!(names(&built, false), ALL_UNITS, "cold: {built:?}");
    assert_eq!(repo.recycled(), Vec::<Value>::new());
    assert!(a_dir.is_dir(), "the orphan stays");
}

/// Keeps anything from being written into the directory `dir` until
/// dropped (what is in it can still be read): on Windows an entry of its
/// DACL that denies the user adding files there, elsewhere its mode
/// read-only.
struct Unwritable {
    #[cfg(windows)]
    _denied: Denied,
    #[cfg(unix)]
    dir: PathBuf,
}

impl Unwritable {
    fn new(dir: &Path) -> Self {
        #[cfg(windows)]
        let denied = Denied::new(dir, "WD,AD");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))
                .expect("make it read-only");
        }
        Self {
            #[cfg(windows)]
            _denied: denied,
            #[cfg(unix)]
            dir: dir.to_path_buf(),
        }
    }
}

#[cfg(unix)]
impl Drop for Unwritable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o755));
    }
}

/// Denies the user, on Windows, the access rights `rights` (as `icacls`
/// names them: `WA` to write attributes, `WD,AD` to add files to a
/// directory) to `path` until dropped, by an entry of its DACL.
#[cfg(windows)]
struct Denied {
    path: PathBuf,
}

#[cfg(windows)]
impl Denied {
    fn new(path: &Path, rights: &str) -> Self {
        let entry = format!("*{}:({rights})", support::endpoint::current_user_sid());
        let output = Command::new("icacls")
            .arg(path)
            .args(["/deny", &entry])
            .output()
            .expect("run icacls");
        assert_success(&output, "icacls /deny");
        Self {
            path: path.to_path_buf(),
        }
    }
}

#[cfg(windows)]
impl Drop for Denied {
    fn drop(&mut self) {
        let user = format!("*{}", support::endpoint::current_user_sid());
        let _ = Command::new("icacls")
            .arg(&self.path)
            .args(["/remove:d", &user])
            .output();
    }
}

/// A recycler claims its donor before it moves it: the donor's owner
/// manifest names the new workspace first, so no directory at a worktree's
/// place ever names a removed worktree, which another recycler would take
/// for an orphan and move away. A donor whose manifest no recycler can
/// write (here, nothing can be written into its `.heli/`) is claimed by
/// none: it stays where it was, the orphan of its removed worktree, until a
/// recycler can claim it.
#[test]
fn a_donor_is_claimed_before_it_moves() {
    let repo = Repo::new();
    let b = repo.add_worktree("b");
    let c = repo.add_worktree("c");
    let gone = repo.fixture.workspace().join("gone");
    let donor = repo.craft(
        "c0000000000000",
        "donor",
        Some(manifest(&gone, Some(&repo.repository()), 1)),
    );
    let unwritable = Unwritable::new(&donor.join(".heli"));

    let output = repo.recycle_in(&b);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("cargo-heli: no build directory recycled: cannot claim "),
        "{stderr}"
    );
    repo.recycle_in(&c);
    assert_eq!(marker(&donor), "donor", "the donor stays where it was");
    assert!(!named_build_dir(&repo, &b).exists(), "b got nothing");
    assert!(!named_build_dir(&repo, &c).exists(), "c got nothing");
    assert!(
        owner(&donor).is_some_and(|owner| owner["workspace"]
            .as_str()
            .is_some_and(|path| Path::new(path) == gone)),
        "it names its removed worktree still"
    );

    drop(unwritable);
    repo.recycle_in(&c);
    assert_eq!(marker(&repo.build_dir_of(&c)), "donor");
    assert!(!named_build_dir(&repo, &b).exists(), "b got nothing");
}

/// A file of the new worktree that needs a time newer than the donor's last
/// build and cannot get one (here its DACL denies the user setting its
/// attributes) makes cargo rebuild every unit: precise recycling never
/// leaves a file looking older than what was built from another content.
#[cfg(windows)]
#[test]
fn a_file_that_cannot_get_a_newer_time_rebuilds_every_unit() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build_changed_and_remove(&a, true);
    let main = repo.main();
    set_mtimes(&main, an_hour_ago());
    let held = Denied::new(&main.join("leaf").join("src").join("lib.rs"), "WA");
    repo.recycle_in(&main);
    drop(held);
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");

    let output = repo.build_output(&main);
    assert_eq!(leaf_says(&output.stdout, &main), LEAF_FROM_THE_COMMIT);
    let built = units(&output.stdout);
    assert_eq!(names(&built, false), ALL_UNITS, "{built:?}");
}

/// a's `leaf` changes to other content of the same size in the second in
/// which a's index was last written (here by a `git add`), as an agent's
/// edit right after `git worktree add` does, and a builds it. The record of
/// a's build directory holds a's change, not the content git's stat cache
/// remembers (see the result store's test of the same name): the main
/// worktree, whose `leaf` is the commit's, recycles a's build directory
/// precisely and builds `leaf` from its own source.
#[test]
fn a_change_in_the_second_of_the_index_is_recorded_by_its_content() {
    let repo = Repo::new();
    let a = repo.add_worktree("a");
    repo.build(&a);
    let lib = a.join("leaf").join("src").join("lib.rs");
    let original = std::fs::read_to_string(&lib).expect("read leaf's source");
    let changed = original.replace("leaf from the commit", "leaf from a's change");
    assert_eq!(changed.len(), original.len(), "the same size");
    let index = repo.git(
        &a,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    let index = PathBuf::from(text(&index.stdout).trim_end());
    support::within_one_second(&[lib.clone(), index], || {
        std::fs::write(&lib, &original).expect("write leaf's source");
        repo.git(&a, &["add", "leaf/src/lib.rs"]);
        std::fs::write(&lib, &changed).expect("change leaf's source");
    });
    let output = repo.build_output(&a);
    assert_eq!(
        leaf_says(&output.stdout, &a),
        "leaf: leaf from a's change over root from the commit"
    );
    let main = repo.main();
    set_mtimes(&main, an_hour_ago());
    repo.fixture.kill_daemon();
    repo.remove_worktree("a");

    let output = repo.build_output(&main);
    assert_eq!(leaf_says(&output.stdout, &main), LEAF_FROM_THE_COMMIT);
    let recycled = repo.recycled();
    assert_eq!(recycled.len(), 1, "{recycled:?}");
    assert_eq!(recycled[0]["mode"], "precise", "{recycled:?}");
}
