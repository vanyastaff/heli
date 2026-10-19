//! Single-flight (docs/runs.md, lifecycle step 3): a verify or test run
//! whose key matches a run in flight anywhere on the machine attaches to
//! that run, from any worktree, instead of starting another. Its client gets
//! the run's output from its first byte and its exit code.

mod support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use support::{Fixture, assert_success, runs};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

/// A fixture repository with a second worktree of the same commit, and a
/// fake toolchain outside both, so that the same check in either has the
/// same key. Its cargo is a copy of `echo-args` (it prints its arguments,
/// counts its starts, waits for the gate, and exits 0 unless told
/// otherwise), named `fake-cargo` so that the build store does not ask it
/// for metadata (docs/build-store.md#accounting), which would count as a start;
/// its `rustc` is another copy. Both answer version requests as
/// [`Twins::set_version`] sets them.
struct Twins {
    fixture: Fixture,
    /// The second worktree.
    other: PathBuf,
    cargo: PathBuf,
    /// The search path, the fake `rustc` first.
    path: OsString,
}

impl Twins {
    fn new() -> Self {
        let fixture = Fixture::new("basic");
        let echo = fixture.echo_args();
        let workspace = fixture.workspace();
        git(&workspace, &["init", "--quiet"]);
        git(&workspace, &["config", "core.autocrlf", "false"]);
        git(&workspace, &["add", "-A"]);
        git(
            &workspace,
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
        let other = fixture.path("other");
        git(
            &workspace,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                other.to_str().expect("a UTF-8 path"),
                "HEAD",
            ],
        );
        let tools = fixture.path("tools");
        std::fs::create_dir_all(&tools).expect("create the tools");
        let tool = |name: &str| {
            let path = tools.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            support::copy_program(&echo, &path);
            path
        };
        let cargo = tool("fake-cargo");
        tool("rustc");
        let path = std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .expect("a search path");
        std::fs::create_dir_all(fixture.path("versions")).expect("create the versions");
        std::fs::create_dir_all(fixture.path("cargo-home")).expect("create the Cargo home");
        let twins = Self {
            fixture,
            other,
            cargo,
            path,
        };
        twins.set_version("fake-cargo", "cargo 1.0.0");
        twins.set_version("rustc", "rustc 1.0.0");
        twins
    }

    /// The first worktree.
    fn workspace(&self) -> PathBuf {
        self.fixture.workspace()
    }

    /// Sets the version the fake tools report for `request` (`fake-cargo`,
    /// `rustc`).
    fn set_version(&self, request: &str, version: &str) {
        std::fs::write(self.fixture.path("versions").join(request), version)
            .expect("write a version");
    }

    /// `cargo heli <args>` in `dir`, with the fake toolchain; the command
    /// waits for the gate.
    fn heli(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = self.fixture.heli(args.iter().copied());
        command
            .current_dir(dir)
            .env("CARGO", &self.cargo)
            // This fixture selects its fake compiler through PATH, even
            // when the real suite was launched with an explicit RUSTC.
            .env_remove("RUSTC")
            .env("CARGO_HOME", self.fixture.path("cargo-home"))
            .env("ECHO_VERSION", self.fixture.path("versions"))
            .env("PATH", &self.path)
            .env("ECHO_WAIT_FOR", self.fixture.path("gate"))
            .env("ECHO_STARTS", self.fixture.path("starts"));
        command
    }

    /// `cargo heli check` in `dir`.
    fn check(&self, dir: &Path) -> Command {
        self.heli(dir, &["check"])
    }

    fn open_gate(&self) {
        std::fs::write(self.fixture.path("gate"), "").expect("open the gate");
    }

    /// How many times the fake cargo has run a command.
    fn starts(&self) -> usize {
        std::fs::read_to_string(self.fixture.path("starts"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

/// Starts `command` with its output captured.
fn start(command: &mut Command) -> Child {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli")
}

/// Waits for `child` to exit, at most a minute: a wait that is never
/// released fails the test instead of hanging it.
fn finish(child: Child) -> Output {
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(child.wait_with_output()));
    match receiver.recv_timeout(Duration::from_secs(60)) {
        Ok(output) => output.expect("wait for heli"),
        Err(_) => {
            support::kill_tree(pid);
            panic!("heli (pid {pid}) did not finish within a minute");
        }
    }
}

/// The notice line an attached client prints first on stderr, if any.
fn notice(output: &Output) -> Option<String> {
    let stderr = support::without_footer(&output.stderr);
    let first = stderr.lines().next()?;
    first
        .starts_with("cargo-heli: attached to ")
        .then(|| first.to_owned())
}

/// The run `status` lists as `run`.
fn id(run: &Value) -> String {
    run["id"].as_str().expect("an id").to_owned()
}

/// The run a client followed: the one its footer names.
fn run_of(output: &Output) -> String {
    support::split_footer(&output.stderr).1.run
}

/// Waits until `status` lists `count` runs, and returns them, oldest first.
fn wait_for_runs(fixture: &Fixture, count: usize) -> Vec<Value> {
    let status = fixture.wait_for_status(&format!("{count} runs"), |status| {
        runs(status).len() == count
    });
    runs(&status)
}

/// Waits until run `run` has `count` clients.
fn wait_for_clients(fixture: &Fixture, run: &str, count: u32) {
    fixture.wait_for_status(&format!("{count} clients of {run}"), |status| {
        runs(status)
            .iter()
            .any(|listed| listed["id"] == run && listed["attached_clients"] == count)
    });
}

/// Waits until run `run` has printed something: its command has started.
fn wait_for_output(fixture: &Fixture, run: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let output = fixture.heli(["log", run]).output().expect("run heli log");
        if !output.stdout.is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the output of {run}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A file tracked in both worktrees.
fn library(worktree: &Path) -> PathBuf {
    worktree
        .join("crates")
        .join("slow-a")
        .join("src")
        .join("lib.rs")
}

/// Edits [`library`] in `worktree` as the fake `fmt` of
/// [`a_run_waiting_for_its_worktree_is_keyed_again`] does: one more line.
fn edit(worktree: &Path) {
    let mut text = std::fs::read(library(worktree)).expect("read the library");
    text.extend_from_slice(b"started\n");
    std::fs::write(library(worktree), text).expect("edit the library");
}

/// A run whose key differs in any keyed input starts on its own: the
/// arguments, the directory within the worktree, a variable, the Cargo
/// configuration, the toolchain or the tree. One that differs only in what
/// the key leaves out (here the target directory, from another worktree) is
/// the same run, and attaches.
#[test]
fn a_run_whose_key_differs_starts_on_its_own() {
    let twins = Twins::new();
    let other = &twins.other;
    let first = start(&mut twins.check(&twins.workspace()));
    let first_run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    let mut same = twins.check(other);
    same.env("CARGO_TARGET_DIR", twins.fixture.path("other-target"));
    let same = start(&mut same);
    wait_for_clients(&twins.fixture, &first_run, 2);

    let other_home = twins.fixture.path("other-cargo-home");
    std::fs::create_dir_all(&other_home).expect("create a Cargo home");
    std::fs::write(
        other_home.join("config.toml"),
        "[build]\nincremental = false\n",
    )
    .expect("write a Cargo configuration");
    let mut variable = twins.check(other);
    variable.env("SINGLE_FLIGHT_TEST", "1");
    let mut configuration = twins.check(other);
    configuration.env("CARGO_HOME", &other_home);
    let mut differing = Vec::new();
    for (input, mut command) in [
        (
            "the arguments",
            twins.heli(other, &["check", "--all-targets"]),
        ),
        ("the directory", twins.check(&other.join("crates"))),
        ("a variable", variable),
        ("the Cargo configuration", configuration),
    ] {
        differing.push((input, start(&mut command)));
        wait_for_runs(&twins.fixture, differing.len() + 1);
    }
    // The first run was keyed with rustc 1.0.0.
    twins.set_version("rustc", "rustc 2.0.0");
    differing.push(("the toolchain", start(&mut twins.check(other))));
    wait_for_runs(&twins.fixture, differing.len() + 1);
    twins.set_version("rustc", "rustc 1.0.0");
    edit(other);
    differing.push(("the tree", start(&mut twins.check(other))));
    let listed = wait_for_runs(&twins.fixture, differing.len() + 1);
    assert_eq!(listed[0]["attached_clients"], 2, "{listed:?}");

    twins.open_gate();
    let first = finish(first);
    assert_success(&first, "the first check");
    assert_eq!(run_of(&first), first_run);
    let same = finish(same);
    assert_success(&same, "the same check");
    assert_eq!(run_of(&same), first_run);
    assert!(notice(&same).is_some_and(|notice| notice.contains(&first_run)));
    let mut runs = vec![first_run];
    for (input, call) in differing {
        let output = finish(call);
        assert_success(&output, input);
        assert_eq!(notice(&output), None, "{input}");
        runs.push(run_of(&output));
    }
    runs.sort();
    runs.dedup();
    assert_eq!(
        runs.len(),
        7,
        "each differing check ran on its own: {runs:?}"
    );
    assert_eq!(twins.starts(), 7);
}

/// The same command in the same worktree is not the same run once the tree
/// has changed: called again after an edit, it starts a run of its own
/// rather than attach to one that started on the tree before the edit.
/// Called again before the edit, it attaches.
#[test]
fn the_same_check_after_an_edit_runs_anew() {
    let twins = Twins::new();
    let workspace = twins.workspace();
    let first = start(&mut twins.check(&workspace));
    let first_run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    let again = start(&mut twins.check(&workspace));
    wait_for_clients(&twins.fixture, &first_run, 2);
    edit(&workspace);
    let edited = start(&mut twins.check(&workspace));
    let listed = wait_for_runs(&twins.fixture, 2);
    assert_eq!(listed[1]["attached_clients"], 1, "{listed:?}");

    twins.open_gate();
    let (first, again, edited) = (finish(first), finish(again), finish(edited));
    for (output, what) in [(&first, "first"), (&again, "again"), (&edited, "edited")] {
        assert_success(output, what);
    }
    assert_eq!(run_of(&again), first_run);
    assert!(notice(&again).is_some_and(|notice| notice.contains(&first_run)));
    assert_ne!(run_of(&edited), first_run);
    assert_eq!(notice(&edited), None);
    assert_eq!(twins.starts(), 2);
}

/// The markers in the fixture's result store.
fn markers(fixture: &Fixture) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(fixture.state_dir().join("verified")) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.expect("an entry").path())
        .collect()
}

/// When the run fails, every client attached to it gets the failure (the
/// run's output and its exit code), and nothing is recorded: the same check
/// runs again.
#[test]
fn a_failure_reaches_every_client_and_is_not_recorded() {
    failure_reaches_every_client("on");
}

#[test]
fn cache_off_failure_reaches_every_client_and_is_not_recorded() {
    failure_reaches_every_client("off");
}

fn failure_reaches_every_client(cache: &str) {
    let twins = Twins::new();
    let failing = |dir: &Path| {
        let mut command = twins.heli(dir, &["--cache", cache, "check"]);
        command.env("ECHO_EXIT", "1");
        command
    };
    let first = start(&mut failing(&twins.workspace()));
    let run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    let second = start(&mut failing(&twins.other));
    wait_for_clients(&twins.fixture, &run, 2);

    twins.open_gate();
    let (first, second) = (finish(first), finish(second));
    for output in [&first, &second] {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            text(&output.stdout).starts_with("arg: \"check\"\n"),
            "{output:?}"
        );
        assert!(
            support::without_footer(&output.stderr).ends_with("echo-args: stderr\n"),
            "{output:?}"
        );
        assert_eq!(run_of(output), run);
    }
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(markers(&twins.fixture), Vec::<PathBuf>::new());
    let again = failing(&twins.other).output().expect("run heli");
    assert_eq!(again.status.code(), Some(1), "it runs again: {again:?}");
    assert_eq!(twins.starts(), 2);
}

/// Killing a client (as an agent harness stops a command) stops that client
/// only: the run goes on for the others, even when the one killed started
/// it, and they get its whole output and its exit code.
#[test]
fn killing_one_client_leaves_the_run_to_the_others() {
    let twins = Twins::new();
    let starter = start(&mut twins.check(&twins.workspace()));
    let run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    let others = [
        start(&mut twins.check(&twins.other)),
        start(&mut twins.check(&twins.other)),
    ];
    wait_for_clients(&twins.fixture, &run, 3);
    support::kill_tree(starter.id());
    let _ = finish(starter);
    wait_for_clients(&twins.fixture, &run, 2);

    twins.open_gate();
    for other in others {
        let output = finish(other);
        assert_success(&output, "an attached check");
        assert!(
            text(&output.stdout).starts_with("arg: \"check\"\n"),
            "{output:?}"
        );
        assert_eq!(run_of(&output), run);
    }
    assert_eq!(twins.starts(), 1, "the check ran once");
}

/// A run that waits at its worktree lock is keyed again once its wait is
/// over, since the runs it waited for may have changed its tree (a `fmt`).
/// Until then it takes the clients of its key from its own worktree only,
/// where such a change reaches the clients' tree too; a client in another
/// worktree runs on its own rather than get the verdict of a tree it does
/// not have. After the wait, the run takes the clients of the key it has
/// then, from any worktree.
#[test]
fn a_run_waiting_for_its_worktree_is_keyed_again() {
    waiting_run_is_keyed_again("on");
}

#[test]
fn cache_off_waiting_for_its_worktree_is_keyed_again() {
    waiting_run_is_keyed_again("off");
}

fn waiting_run_is_keyed_again(cache: &str) {
    let twins = Twins::new();
    let workspace = twins.workspace();
    let check = |dir: &Path| twins.heli(dir, &["--cache", cache, "check"]);
    let build_gate = twins.fixture.path("build-gate");
    let mut build = twins.heli(&workspace, &["build"]);
    build.env("ECHO_WAIT_FOR", &build_gate);
    let build = start(&mut build);
    wait_for_runs(&twins.fixture, 1);
    // `fmt` waits for the build, then edits the library: the fake cargo
    // appends a line to its `ECHO_STARTS`.
    let mut fmt = twins.heli(&workspace, &["fmt"]);
    fmt.env("ECHO_STARTS", library(&workspace))
        .env_remove("ECHO_WAIT_FOR");
    let fmt = start(&mut fmt);
    wait_for_runs(&twins.fixture, 2);
    let checked = start(&mut check(&workspace));
    let listed = wait_for_runs(&twins.fixture, 3);
    assert!(listed[2]["waiting_since"].is_u64(), "{listed:?}");
    let check_run = id(&listed[2]);

    let here = start(&mut check(&workspace));
    wait_for_clients(&twins.fixture, &check_run, 2);
    let there = start(&mut check(&twins.other));
    wait_for_runs(&twins.fixture, 4);

    std::fs::write(&build_gate, "").expect("open the build's gate");
    wait_for_output(&twins.fixture, &check_run);
    edit(&twins.other);
    let changed = start(&mut check(&twins.other));
    wait_for_clients(&twins.fixture, &check_run, 3);

    twins.open_gate();
    assert_success(&finish(build), "the build");
    assert_success(&finish(fmt), "fmt");
    let checked = finish(checked);
    assert_success(&checked, "the check");
    assert_eq!(run_of(&checked), check_run);
    let here = finish(here);
    assert_success(&here, "the check in the same worktree");
    assert_eq!(run_of(&here), check_run);
    let there = finish(there);
    assert_success(&there, "the check in the other worktree");
    assert_ne!(run_of(&there), check_run);
    assert_eq!(notice(&there), None);
    let changed = finish(changed);
    assert_success(&changed, "the check in the other worktree, changed alike");
    assert_eq!(run_of(&changed), check_run);
    let notice = notice(&changed).expect("a notice");
    assert!(notice.contains(&check_run), "{notice}");
    // The build, the check and the check in the other worktree.
    assert_eq!(twins.starts(), 3);
}

/// `--no-cache` runs a check anew: it attaches to no run of its key in
/// flight. A run so forced still takes the clients of its key, once it has
/// one.
#[test]
fn no_cache_runs_anew() {
    let twins = Twins::new();
    let forced = || twins.heli(&twins.workspace(), &["--no-cache", "check"]);
    let first = start(&mut forced());
    let first_run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    wait_for_output(&twins.fixture, &first_run);
    let check = start(&mut twins.check(&twins.other));
    wait_for_clients(&twins.fixture, &first_run, 2);
    let again = start(&mut forced());
    wait_for_runs(&twins.fixture, 2);

    twins.open_gate();
    let (first, check, again) = (finish(first), finish(check), finish(again));
    for (output, what) in [(&first, "first"), (&check, "check"), (&again, "again")] {
        assert_success(output, what);
    }
    assert_eq!(run_of(&check), first_run);
    assert!(notice(&check).is_some_and(|notice| notice.contains(&first_run)));
    assert_ne!(run_of(&again), first_run);
    assert_eq!(notice(&again), None);
    assert_eq!(twins.starts(), 2);
}

#[test]
fn exec_verification_shares_only_unchanged_inputs() {
    let twins = Twins::new();
    let command = || {
        twins.heli(
            &twins.workspace(),
            &[
                "exec",
                "--verify",
                "--",
                twins.cargo.to_str().expect("cargo path"),
                "check",
            ],
        )
    };
    let first = start(&mut command());
    let run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    wait_for_output(&twins.fixture, &run);
    let second = start(&mut command());
    wait_for_clients(&twins.fixture, &run, 2);
    edit(&twins.workspace());
    let changed = start(&mut command());
    wait_for_runs(&twins.fixture, 2);
    let different = start(command().env("VERIFICATION_INPUT", "different"));
    wait_for_runs(&twins.fixture, 3);
    twins.open_gate();
    let first = finish(first);
    let second = finish(second);
    let changed = finish(changed);
    let different = finish(different);
    for output in [&first, &second, &changed, &different] {
        assert_success(output, "explicit verification wrapper");
        assert_eq!(
            text(&output.stdout),
            "arg: \"check\"\n",
            "arguments stay verbatim"
        );
    }
    assert_eq!(run_of(&first), run_of(&second));
    assert_ne!(run_of(&changed), run);
    assert_ne!(run_of(&different), run_of(&changed));
    assert_eq!(twins.starts(), 3);
    assert!(
        markers(&twins.fixture).is_empty(),
        "wrappers default to live sharing only"
    );
}

#[test]
fn real_tests_share_then_verify_a_new_source_generation() {
    real_tests_share_then_verify(false);
}

#[test]
fn isolated_real_tests_share_then_verify_a_new_source_generation() {
    real_tests_share_then_verify(true);
}

fn real_tests_share_then_verify(isolated: bool) {
    for wrapper in [false, true] {
        let fixture = Fixture::new("basic");
        let project = fixture.path("real-test-project");
        let home = fixture.path("real-cargo-home");
        let receipts = fixture.path("test-receipts");
        let gate = fixture.path("test-gate");
        for path in [project.join("src"), home.clone(), receipts.clone()] {
            std::fs::create_dir_all(path).expect("create real test fixture");
        }
        std::fs::write(project.join("Cargo.toml"),
            "[workspace]\n[package]\nname='shared-verification-proof'\nversion='0.0.0'\nedition='2024'\n").unwrap();
        std::fs::write(project.join(".gitignore"), "/target/\n").unwrap();
        let source = r#"
const REVISION: &str = "one";
#[test]
fn verifies() {
    let root = std::path::PathBuf::from(std::env::var_os("SHARED_TEST_RECEIPTS").unwrap());
    std::fs::write(root.join(std::process::id().to_string()), REVISION).unwrap();
    println!("revision={REVISION}");
    let gate = std::path::PathBuf::from(std::env::var_os("SHARED_TEST_GATE").unwrap());
    let start = std::time::Instant::now();
    while !gate.exists() {
        assert!(start.elapsed().as_secs() < 60);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
"#;
        std::fs::write(project.join("src/lib.rs"), source).unwrap();
        let lock = fixture
            .cargo(["generate-lockfile", "--offline"])
            .current_dir(&project)
            .env("CARGO_HOME", &home)
            .output()
            .unwrap();
        assert_success(&lock, "generate real fixture lockfile");
        git(&project, &["init", "--quiet"]);
        git(&project, &["config", "core.autocrlf", "false"]);
        git(&project, &["add", "-A"]);
        git(
            &project,
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
                "real test fixture",
            ],
        );
        let command = || {
            let mut command = fixture.heli(["--cache", "off", "--output", "full"]);
            if isolated {
                command.args(["--cargo-home", "isolated"]);
            }
            if wrapper {
                command
                    .args(["exec", "--verify", "--"])
                    .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            }
            command
                .args(["test", "--offline", "--lib", "--", "--nocapture"])
                .current_dir(&project)
                .env("CARGO_HOME", &home)
                .env("SHARED_TEST_RECEIPTS", &receipts)
                .env("SHARED_TEST_GATE", &gate);
            command
        };
        let first = start(&mut command());
        let run = id(&wait_for_runs(&fixture, 1)[0]);
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::fs::read_dir(&receipts).unwrap().count() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "real test did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let second = start(&mut command());
        wait_for_clients(&fixture, &run, 2);
        assert_eq!(std::fs::read_dir(&receipts).unwrap().count(), 1);
        std::fs::write(
            project.join("src/lib.rs"),
            source.replace("\"one\"", "\"two\""),
        )
        .unwrap();
        let changed = start(&mut command());
        let listed = wait_for_runs(&fixture, 2);
        std::fs::write(gate, "").unwrap();
        let outputs = [finish(first), finish(second), finish(changed)];
        for output in &outputs {
            assert_success(output, "real source-aware shared tests");
        }
        assert_eq!(listed[1]["waiting_for"], serde_json::json!([run]));
        assert_eq!(run_of(&outputs[0]), run_of(&outputs[1]));
        assert_ne!(run_of(&outputs[0]), run_of(&outputs[2]));
        assert_eq!(outputs[0].stdout, outputs[1].stdout);
        assert!(text(&outputs[0].stdout).contains("revision=one"));
        assert!(text(&outputs[2].stdout).contains("revision=two"));
        let mut revisions = std::fs::read_dir(receipts)
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>();
        revisions.sort();
        assert_eq!(revisions, ["one", "two"]);
        assert!(markers(&fixture).is_empty());
    }
}

#[test]
fn source_generation_barrier_survives_rekey_after_fmt() {
    let twins = Twins::new();
    let workspace = twins.workspace();
    let build_gate = twins.fixture.path("build-gate");
    let fmt_gate = twins.fixture.path("fmt-gate");
    let mut build = twins.heli(&workspace, &["build"]);
    build.env("ECHO_WAIT_FOR", &build_gate);
    let build = start(&mut build);
    wait_for_runs(&twins.fixture, 1);
    let mut fmt = twins.heli(&workspace, &["fmt"]);
    fmt.env("ECHO_WAIT_FOR", &fmt_gate)
        .env("ECHO_STARTS", library(&workspace));
    let fmt = start(&mut fmt);
    let fmt_run = id(&wait_for_runs(&twins.fixture, 2)[1]);
    let first = start(&mut twins.heli(&workspace, &["--cache", "off", "check"]));
    let first_run = id(&wait_for_runs(&twins.fixture, 3)[2]);
    std::fs::write(build_gate, "").expect("release build");
    wait_for_output(&twins.fixture, &fmt_run);
    let later = start(&mut twins.heli(&workspace, &["--cache", "off", "check", "--all-targets"]));
    let later_run_id = id(&wait_for_runs(&twins.fixture, 3)[2]);
    std::fs::write(fmt_gate, "").expect("release formatter");
    wait_for_output(&twins.fixture, &first_run);
    let status = twins.fixture.status();
    let later_run = runs(&status)
        .into_iter()
        .find(|run| run["id"] == later_run_id)
        .expect("later check");
    twins.open_gate();
    assert_success(&finish(build), "build");
    assert_success(&finish(fmt), "formatter");
    assert_success(&finish(first), "first check after rekey");
    assert_success(&finish(later), "later check after rekey");
    assert_eq!(
        later_run["waiting_for"],
        serde_json::json!([first_run]),
        "source barriers fixed at admission must not disappear during rekey: {status}"
    );
}

#[test]
fn same_source_different_checks_do_not_wait_in_one_worktree() {
    let twins = Twins::new();
    let first = start(&mut twins.heli(&twins.workspace(), &["--cache", "off", "check"]));
    let run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    wait_for_output(&twins.fixture, &run);
    let second = start(&mut twins.heli(
        &twins.workspace(),
        &["--cache", "off", "check", "--all-targets"],
    ));
    let listed = wait_for_runs(&twins.fixture, 2);
    twins.open_gate();
    assert_success(&finish(first), "first check");
    assert_success(&finish(second), "other check of the same source state");
    assert!(
        listed[1]["waiting_for"]
            .as_array()
            .expect("blockers")
            .is_empty(),
        "{listed:?}"
    );
    assert_eq!(twins.starts(), 2);
}

#[test]
fn builds_share_only_current_sources_and_the_same_artifact_owner() {
    let twins = Twins::new();
    let command = || twins.heli(&twins.workspace(), &["--cache", "on", "build"]);
    let first = start(&mut command());
    let first_run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    wait_for_output(&twins.fixture, &first_run);
    let same = start(&mut command());
    wait_for_clients(&twins.fixture, &first_run, 2);
    let other = start(&mut twins.heli(&twins.other, &["build"]));
    wait_for_runs(&twins.fixture, 2);
    let mut targets = Vec::new();
    for name in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_BUILD_BUILD_DIR",
    ] {
        let mut target = command();
        target.env(name, twins.fixture.path(name));
        targets.push(start(&mut target));
        wait_for_runs(&twins.fixture, targets.len() + 2);
    }
    let features = start(&mut twins.heli(&twins.workspace(), &["build", "--features", "another"]));
    wait_for_runs(&twins.fixture, 6);
    edit(&twins.workspace());
    let changed = start(&mut command());
    let status = twins
        .fixture
        .wait_for_status("changed build admitted", |status| {
            let listed = runs(status);
            listed.len() == 7 || listed.iter().any(|run| run["attached_clients"] == 3)
        });
    twins.open_gate();
    let outputs = [finish(first), finish(same), finish(other), finish(changed)];
    for output in &outputs {
        assert_success(output, "build");
    }
    assert_eq!(run_of(&outputs[0]), run_of(&outputs[1]));
    assert_ne!(
        run_of(&outputs[0]),
        run_of(&outputs[2]),
        "other worktree needs artifacts"
    );
    assert_ne!(
        run_of(&outputs[0]),
        run_of(&outputs[3]),
        "new sources need a new build: {status}"
    );
    for target in targets {
        let output = finish(target);
        assert_success(&output, "separate build output location");
        assert_ne!(run_of(&outputs[0]), run_of(&output));
    }
    let features = finish(features);
    assert_success(&features, "separate features");
    assert_ne!(run_of(&outputs[0]), run_of(&features));
    assert_eq!(twins.starts(), 7);
    assert!(
        markers(&twins.fixture).is_empty(),
        "builds never save verdicts"
    );
    assert_success(
        &command().output().expect("repeat build"),
        "completed build repetition",
    );
    assert_eq!(
        twins.starts(),
        8,
        "a build must check artifact existence again"
    );
    assert!(markers(&twins.fixture).is_empty());
}

#[test]
fn a_live_client_cannot_inherit_a_queued_runs_saved_verdict() {
    let twins = Twins::new();
    let format_gate = twins.fixture.path("format-gate");
    let mut fmt = twins.heli(&twins.workspace(), &["fmt"]);
    fmt.env("ECHO_WAIT_FOR", &format_gate);
    let fmt = start(&mut fmt);
    let format_run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    wait_for_output(&twins.fixture, &format_run);
    let queued = start(&mut twins.check(&twins.workspace()));
    let queued_id = id(&wait_for_runs(&twins.fixture, 2)[1]);
    // The twin completes while the first worktree is still blocked. The
    // queued run will find this saved verdict at its second lookup.
    twins.open_gate();
    assert_success(
        &twins.check(&twins.other).output().expect("twin check"),
        "twin check",
    );
    assert_eq!(markers(&twins.fixture).len(), 1);
    let live = start(&mut twins.heli(&twins.workspace(), &["--cache", "off", "check"]));
    twins
        .fixture
        .wait_for_status("live client's admission", |status| {
            let listed = runs(status);
            listed.len() == 3 || listed.iter().any(|run| run["attached_clients"] == 2)
        });
    std::fs::write(format_gate, "").expect("release formatter");
    assert_success(&finish(fmt), "formatter");
    assert_success(&finish(queued), "queued cached check");
    let live = finish(live);
    assert_success(&live, "live check");
    assert_ne!(
        run_of(&live),
        queued_id,
        "live request inherited a saved verdict"
    );
    assert_eq!(twins.starts(), 3, "formatter, twin, and actual live check");
}

#[test]
fn real_builds_produce_artifacts_in_each_worktree() {
    real_builds_produce_artifacts(false);
}

#[test]
fn isolated_real_builds_produce_artifacts_in_each_worktree() {
    real_builds_produce_artifacts(true);
}

fn real_builds_produce_artifacts(isolated: bool) {
    let twins = Twins::new();
    let gate = twins.fixture.path("real-build-gate");
    let home = twins.fixture.path("real-build-home");
    std::fs::create_dir_all(&home).unwrap();
    let command = |dir: &Path, target: &Path| {
        let mut command = twins.fixture.heli(["--output", "full"]);
        if isolated {
            command.args(["--cargo-home", "isolated"]);
        }
        command
            .args(["build", "--offline", "-p", "gated"])
            .current_dir(dir)
            .env("CARGO_HOME", &home)
            .env("CARGO_TARGET_DIR", target)
            .env("HELI_FIXTURE_GATE", &gate);
        command
    };
    let target = twins.fixture.path("real-build-target");
    let other_target = twins.fixture.path("other-real-build-target");
    let alternate_target = twins.fixture.path("alternate-real-build-target");
    let first = start(&mut command(&twins.workspace(), &target));
    let run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let log = twins.fixture.heli(["log", &run]).output().unwrap();
        if text(&log.stderr).contains("Compiling gated") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "real build did not start: {log:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let same = start(&mut command(&twins.workspace(), &target));
    wait_for_clients(&twins.fixture, &run, 2);
    let other = start(&mut command(&twins.other, &other_target));
    let alternate = start(&mut command(&twins.workspace(), &alternate_target));
    let listed = wait_for_runs(&twins.fixture, 3);
    std::fs::write(gate, "").unwrap();
    let outputs = [
        finish(first),
        finish(same),
        finish(other),
        finish(alternate),
    ];
    for output in &outputs {
        assert_success(output, "real independent artifact owner");
    }
    assert_eq!(run_of(&outputs[0]), run_of(&outputs[1]));
    assert_ne!(run_of(&outputs[0]), run_of(&outputs[2]));
    assert_ne!(run_of(&outputs[0]), run_of(&outputs[3]));
    assert_ne!(run_of(&outputs[2]), run_of(&outputs[3]));
    for run in &listed[1..] {
        assert_eq!(run["waiting_for"], serde_json::json!([]));
    }
    for target in [&target, &other_target, &alternate_target] {
        assert!(
            target.join("debug/libgated.rlib").is_file(),
            "missing build artifact in {}",
            target.display()
        );
    }
    assert!(markers(&twins.fixture).is_empty());
}

#[test]
fn isolated_real_checks_share_across_worktrees_without_saved_verdicts() {
    let twins = Twins::new();
    let gate = twins.fixture.path("real-check-gate");
    let home = twins.fixture.path("real-check-home");
    std::fs::create_dir_all(&home).unwrap();
    let command = |dir: &Path, target: &Path| {
        let mut command = twins.fixture.heli([
            "--cargo-home",
            "isolated",
            "--cache",
            "off",
            "--output",
            "full",
            "check",
            "--offline",
            "-p",
            "gated",
        ]);
        command
            .current_dir(dir)
            .env("CARGO_HOME", &home)
            .env("CARGO_TARGET_DIR", target)
            .env("HELI_FIXTURE_GATE", &gate);
        command
    };
    let target = twins.fixture.path("real-check-target");
    let other_target = twins.fixture.path("other-real-check-target");
    let first = start(&mut command(&twins.workspace(), &target));
    let run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    twins
        .fixture
        .wait_for_status("running real check", |status| {
            runs(status).iter().any(|run| run["phase"] == "running")
        });
    let second = start(&mut command(&twins.other, &other_target));
    wait_for_clients(&twins.fixture, &run, 2);
    std::fs::write(gate, "").unwrap();
    let outputs = [finish(first), finish(second)];
    for output in &outputs {
        assert_success(output, "live verification across isolated homes");
    }
    assert_eq!(run_of(&outputs[0]), run_of(&outputs[1]));
    assert_eq!(outputs[0].stdout, outputs[1].stdout);
    assert!(!other_target.join("debug/deps").exists());
    assert!(markers(&twins.fixture).is_empty());
}

#[test]
fn different_source_states_wait_within_one_worktree() {
    for cache in ["off", "refresh"] {
        let twins = Twins::new();
        let first = start(&mut twins.heli(&twins.workspace(), &["--cache", cache, "check"]));
        let run = id(&wait_for_runs(&twins.fixture, 1)[0]);
        wait_for_output(&twins.fixture, &run);
        edit(&twins.workspace());
        let changed = start(&mut twins.heli(
            &twins.workspace(),
            &["--cache", cache, "check", "--all-targets"],
        ));
        let listed = wait_for_runs(&twins.fixture, 2);
        twins.open_gate();
        assert_success(&finish(first), "old source check");
        assert_success(&finish(changed), "new source check");
        assert_eq!(
            listed[1]["waiting_for"],
            serde_json::json!([run]),
            "different source generations must not rewrite live binaries: {listed:?}"
        );
        assert_eq!(twins.starts(), 2);
    }
}

#[test]
fn cache_off_source_preparation_is_not_a_dead_daemon_timeout() {
    let twins = Twins::new();
    let metadata_gate = twins.fixture.path("metadata-gate");
    let metadata_pid = twins.fixture.path("metadata-pid");
    let mut command = twins.heli(&twins.workspace(), &["--cache", "off", "check"]);
    command
        .env("ECHO_METADATA_WAIT_FOR", &metadata_gate)
        .env("ECHO_METADATA_PID", &metadata_pid);
    let first = start(&mut command);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !metadata_pid.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "metadata did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The client bounds its first answer to five seconds. A live daemon
    // preparing a source key must acknowledge it, not trigger direct fallback.
    std::thread::sleep(Duration::from_secs(6));
    let premature_starts = twins.starts();
    std::fs::write(metadata_gate, "").expect("release source inventory");
    twins.open_gate();
    let output = finish(first);
    assert_success(&output, "source-keyed uncached check");
    assert_eq!(
        premature_starts, 0,
        "command ran before its source key was ready"
    );
    assert!(
        !text(&output.stderr).contains("running the command directly"),
        "{output:?}"
    );
    assert_eq!(twins.starts(), 1);
}

#[test]
fn cache_off_shares_the_same_check_without_recording_a_verdict() {
    let twins = Twins::new();
    let command = || twins.heli(&twins.workspace(), &["--cache", "off", "check"]);
    let first = start(&mut command());
    let first_run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    wait_for_output(&twins.fixture, &first_run);
    let second = start(&mut command());
    let status = twins
        .fixture
        .wait_for_status("second request admitted", |status| {
            let listed = runs(status);
            listed.len() == 2 || listed.iter().any(|run| run["attached_clients"] == 2)
        });
    twins.open_gate();
    let outputs = [finish(first), finish(second)];
    for output in &outputs {
        assert_success(output, "uncached check");
    }
    assert_eq!(
        runs(&status).len(),
        1,
        "identical uncached checks must share: {status}"
    );
    assert_eq!(run_of(&outputs[0]), run_of(&outputs[1]));
    assert_eq!(outputs[0].stdout, outputs[1].stdout);
    assert_eq!(twins.starts(), 1);
    assert!(markers(&twins.fixture).is_empty(), "no persistent verdict");
    let again = command().output().expect("repeat after completion");
    assert_success(&again, "uncached repetition");
    assert_ne!(
        run_of(&again),
        first_run,
        "completed verdict must not be reused"
    );
    assert_eq!(twins.starts(), 2);
    assert!(markers(&twins.fixture).is_empty());
}

#[test]
fn cache_off_never_attaches_to_a_check_of_earlier_sources() {
    let twins = Twins::new();
    let command = || twins.heli(&twins.workspace(), &["--cache", "off", "check"]);
    let first = start(&mut command());
    let first_run = id(&wait_for_runs(&twins.fixture, 1)[0]);
    wait_for_output(&twins.fixture, &first_run);
    edit(&twins.workspace());
    let again = start(&mut command());
    let status = twins
        .fixture
        .wait_for_status("second client's admission", |status| {
            let listed = runs(status);
            listed.len() == 2 || listed.iter().any(|run| run["attached_clients"] == 2)
        });
    let listed = runs(&status);
    twins.open_gate();
    let first = finish(first);
    let again = finish(again);
    assert_success(&first, "first check");
    assert_success(&again, "changed check");
    assert_eq!(
        listed.len(),
        2,
        "uncached check reused an older verdict: {status}"
    );
    assert_ne!(run_of(&first), run_of(&again));
    assert_eq!(notice(&again), None);
    assert_eq!(twins.starts(), 2);
}

/// Outside a git repository a check has no key: nothing tells whether its
/// tree is the same as a run's in flight, so it attaches to none.
#[test]
fn a_check_without_a_key_attaches_to_no_run() {
    check_without_a_key_attaches_to_no_run("on");
}

#[test]
fn cache_off_without_a_key_attaches_to_no_run() {
    check_without_a_key_attaches_to_no_run("off");
}

fn check_without_a_key_attaches_to_no_run(cache: &str) {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let check = || {
        let mut check = fixture.heli(["--cache", cache, "check"]);
        check
            .env("CARGO", &echo)
            .env("ECHO_WAIT_FOR", fixture.path("gate"))
            .env("ECHO_STARTS", fixture.path("starts"));
        start(&mut check)
    };
    let first = check();
    wait_for_runs(&fixture, 1);
    let second = check();
    let listed = wait_for_runs(&fixture, 2);
    assert_eq!(listed[0]["attached_clients"], 1, "{listed:?}");

    std::fs::write(fixture.path("gate"), "").expect("open the gate");
    let (first, second) = (finish(first), finish(second));
    for output in [&first, &second] {
        assert_success(output, "cargo heli check");
        assert_eq!(notice(output), None);
    }
    assert_ne!(run_of(&first), run_of(&second));
}

/// The same check started at the same moment in two worktrees with the same
/// tree runs once: one client starts the run, the other attaches to it
/// (one notice line naming the run and its worktree), and both get the
/// run's whole output and its exit code.
#[test]
fn the_same_check_in_two_worktrees_runs_once() {
    same_check_in_two_worktrees_runs_once("on");
}

#[test]
fn cache_off_same_check_in_two_worktrees_runs_once() {
    same_check_in_two_worktrees_runs_once("off");
}

fn same_check_in_two_worktrees_runs_once(cache: &str) {
    let twins = Twins::new();
    let first = start(&mut twins.heli(&twins.workspace(), &["--cache", cache, "check"]));
    let second = start(&mut twins.heli(&twins.other, &["--cache", cache, "check"]));
    let status = twins
        .fixture
        .wait_for_status("one run with two clients", |status| {
            let runs = runs(status);
            runs.len() == 1 && runs[0]["attached_clients"] == 2
        });
    let run = runs(&status)[0].clone();

    twins.open_gate();
    let outputs = [finish(first), finish(second)];
    for output in &outputs {
        assert_success(output, "cargo heli check");
        assert!(
            text(&output.stdout).starts_with("arg: \"check\"\n"),
            "{output:?}"
        );
        assert_eq!(output.stdout, outputs[0].stdout);
        assert_eq!(support::split_footer(&output.stderr).1.run, id(&run));
    }
    let notices: Vec<String> = outputs.iter().filter_map(notice).collect();
    let [notice] = &notices[..] else {
        panic!("one client attached: {notices:?}");
    };
    assert!(notice.contains(&id(&run)), "{notice}");
    let worktree = run["worktree"].as_str().expect("a worktree");
    assert!(notice.contains(worktree), "names {worktree}: {notice}");
    assert_eq!(twins.starts(), 1, "the check ran once");
}

/// How many times the fake `rustc` was asked its version, as logged in
/// `log`: once each time a run's key was computed.
fn keyings(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == "rustc")
        .count()
}

/// Single-flight matches the key the result store's first lookup computed,
/// so a miss costs no key of its own; and a run that waits for nothing
/// keeps that key, the one its clients matched, rather than compute it
/// again once admitted. A run that waits at its worktree lock is keyed
/// again after the wait. Each passing run is keyed once more after its
/// steps, every input read anew, to be recorded only under an unchanged
/// key (docs/result-store.md).
#[test]
fn a_run_that_waits_for_nothing_is_keyed_once() {
    let twins = Twins::new();
    let workspace = twins.workspace();
    let log = twins.fixture.path("asked");
    let check = || {
        let mut check = twins.check(&workspace);
        check.env("ECHO_VERSION_LOG", &log);
        check
    };
    twins.open_gate();
    assert_success(&check().output().expect("run heli"), "the check");
    assert_eq!(
        keyings(&log),
        2,
        "a check that waits for nothing, and after it"
    );

    std::fs::remove_file(&log).expect("clear the log");
    edit(&workspace);
    let fmt_gate = twins.fixture.path("fmt-gate");
    let mut fmt = twins.heli(&workspace, &["fmt"]);
    fmt.env("ECHO_WAIT_FOR", &fmt_gate);
    let fmt = start(&mut fmt);
    wait_for_runs(&twins.fixture, 1);
    let waiting = start(&mut check());
    let listed = wait_for_runs(&twins.fixture, 2);
    assert!(listed[1]["waiting_since"].is_u64(), "{listed:?}");
    std::fs::write(&fmt_gate, "").expect("open fmt's gate");
    assert_success(&finish(fmt), "fmt");
    assert_success(&finish(waiting), "the check that waited");
    assert_eq!(
        keyings(&log),
        3,
        "a check that waited for fmt, and after it"
    );
}
