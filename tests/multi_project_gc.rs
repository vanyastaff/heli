//! Public CLI acceptance for stores shared by outer and nested Cargo runs.

mod support;

use serde_json::Value;
use support::{Fixture, assert_success};

fn configured() -> (Fixture, std::path::PathBuf) {
    let fixture = Fixture::new("basic");
    let store = fixture.path("store");
    fixture.configure(&format!(
        "[store]\npaths = ['{}']\nbudget_mb = 16\nunit_hours = 0\n\
         orphan_hours = 0\nidle_hours = 0\ndir_hours = 0\n",
        store.display()
    ));
    (fixture, store)
}

fn nested_build(fixture: &Fixture, store: &std::path::Path) {
    let output = fixture
        .heli(["--no-cache", "test", "--quiet", "-p", "nested"])
        .env("CARGO_BUILD_BUILD_DIR", store.join("{workspace-path-hash}"))
        .env("CARGO_INCREMENTAL", "0")
        .env("HELI_FIXTURE_ARTIFACT_MB", "64")
        .output()
        .expect("run outer and nested cargo");
    assert_success(&output, "nested cargo workload");
}

fn gc(fixture: &Fixture) -> Value {
    let output = fixture.heli(["gc", "--json"]).output().expect("run GC");
    assert_success(&output, "GC");
    serde_json::from_slice(&output.stdout).expect("GC JSON")
}

fn survey(fixture: &Fixture) -> Value {
    let output = fixture
        .heli(["gc", "--dry-run", "--json"])
        .output()
        .expect("survey");
    assert_success(&output, "survey");
    serde_json::from_slice(&output.stdout).expect("survey JSON")
}

#[test]
fn nested_cargo_artifacts_can_be_reclaimed_under_the_shared_budget() {
    let (fixture, store) = configured();
    nested_build(&fixture, &store);
    gc(&fixture);
    let listing = survey(&fixture);
    assert!(
        listing["used"].as_u64().expect("accounted bytes") <= 16 << 20,
        "completed nested Cargo left unreclaimable artifacts above budget: {listing}"
    );
}

#[test]
fn an_existing_store_is_not_adopted_even_when_initially_empty() {
    let (fixture, store) = configured();
    std::fs::create_dir(&store).expect("preexisting store");
    nested_build(&fixture, &store);
    let listing = gc(&fixture);
    assert!(
        listing["used"].as_u64().expect("bytes") >= 64 << 20,
        "{listing}"
    );
    assert!(
        listing["dirs"]
            .as_array()
            .expect("dirs")
            .iter()
            .any(|dir| { dir["state"] == "unknown" && dir["managed_cache_root"] == false }),
        "{listing}"
    );
}

#[test]
fn dry_run_never_creates_or_claims_a_store() {
    let (fixture, store) = configured();
    let output = fixture
        .heli(["gc", "--dry-run", "--json"])
        .output()
        .expect("dry run");
    assert_success(&output, "dry run");
    assert!(!store.exists(), "dry-run created a cache root");
}

#[test]
fn a_broken_ownership_marker_does_not_authorize_unknown_eviction() {
    let (fixture, store) = configured();
    fixture.start_daemon();
    let marker = store.join(".heli-cache-root.json");
    let mut value: Value =
        serde_json::from_slice(&std::fs::read(&marker).expect("marker")).expect("valid marker");
    value["schema"] = 99.into();
    std::fs::write(marker, value.to_string()).expect("replace the marker");
    nested_build(&fixture, &store);
    let listing = gc(&fixture);
    assert!(
        listing["used"].as_u64().expect("bytes") >= 64 << 20,
        "{listing}"
    );
}

#[test]
fn an_unrecognized_cargo_lock_layout_is_not_authorized_by_store_ownership() {
    let (fixture, store) = configured();
    fixture.start_daemon();
    let directory = store.join("ab").join("1234");
    let deps = directory.join("debug").join("deps");
    std::fs::create_dir_all(&deps).expect("future layout");
    let artifact = deps.join("future-artifact");
    let file = std::fs::File::create(&artifact).expect("artifact");
    file.set_len(64 << 20).expect("size");
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
    file.set_modified(old).expect("old artifact");
    drop(file);
    gc(&fixture);
    assert!(
        artifact.exists(),
        "unknown lock protocol must remain conservative"
    );
}

#[test]
fn gc_preserves_a_busy_nested_cargo_then_reclaims_it_after_completion() {
    let (fixture, store) = configured();
    let gate = fixture.path("inner-gate");
    let mut child = fixture
        .heli(["--no-cache", "test", "--quiet", "-p", "nested"])
        .env("CARGO_BUILD_BUILD_DIR", store.join("{workspace-path-hash}"))
        .env("CARGO_INCREMENTAL", "0")
        .env("HELI_FIXTURE_ARTIFACT_MB", "64")
        .env("HELI_FIXTURE_INNER_GATE", &gate)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start nested build");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let busy = loop {
        let listing = gc(&fixture);
        if let Some(dir) = listing["dirs"]
            .as_array()
            .expect("dirs")
            .iter()
            .find(|dir| {
                dir["workspace"].is_null()
                    && dir["state"] == "busy"
                    && dir["size"].as_u64().is_some_and(|bytes| bytes >= 64 << 20)
            })
        {
            break std::path::PathBuf::from(dir["path"].as_str().expect("path"));
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nested build not busy: {listing}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(busy.exists(), "GC removed a busy child build directory");
    std::fs::write(gate, "open").expect("release child");
    assert!(child.wait().expect("wait for build").success());
    gc(&fixture);
    let listing = survey(&fixture);
    assert!(
        listing["used"].as_u64().expect("bytes") <= 16 << 20,
        "{listing}"
    );
    assert!(!busy.exists(), "completed nested cache was not reclaimed");
}

#[test]
fn a_copied_marker_does_not_claim_another_existing_store() {
    let (fixture, store) = configured();
    fixture.start_daemon();
    let other = fixture.path("other-store");
    let profile = other.join("ab").join("1234").join("debug");
    std::fs::create_dir_all(profile.join("deps")).expect("other store");
    std::fs::copy(
        store.join(".heli-cache-root.json"),
        other.join(".heli-cache-root.json"),
    )
    .expect("copy marker");
    std::fs::File::create(profile.join(".cargo-build-lock")).expect("Cargo lock");
    let artifact = profile.join("deps").join("unrelated-artifact");
    let file = std::fs::File::create(&artifact).expect("artifact");
    file.set_len(64 << 20).expect("size");
    drop(file);
    fixture.configure(&format!(
        "[store]\npaths = ['{}']\nbudget_mb = 16\ndir_hours = 0\n",
        other.display()
    ));
    let listing = gc(&fixture);
    assert!(
        artifact.exists(),
        "a copied marker authorized another cache root"
    );
    assert!(
        listing["used"].as_u64().expect("bytes") >= 64 << 20,
        "{listing}"
    );
}

#[test]
fn a_deep_nested_cargo_lock_protects_the_whole_unknown_directory() {
    let (fixture, store) = configured();
    fixture.start_daemon();
    let directory = store.join("ab").join("1234");
    let profile = directory.join("debug");
    let inner = profile.join("build/pkg/out/child-target/debug");
    std::fs::create_dir_all(&inner).expect("nested Cargo target");
    std::fs::File::create(profile.join(".cargo-build-lock")).expect("outer Cargo lock");
    let lock = std::fs::File::create(inner.join(".cargo-build-lock")).expect("inner lock");
    lock.lock().expect("nested Cargo remains active");
    let artifact = inner.join("artifact");
    std::fs::File::create(&artifact)
        .expect("artifact")
        .set_len(64 << 20)
        .expect("size");
    let output = fixture
        .heli(["gc", "--dry-run", "--json"])
        .output()
        .expect("dry run");
    assert_success(&output, "dry run");
    let listing: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(
        listing["victims"].as_array().expect("victims").is_empty(),
        "GC plans to remove an active nested Cargo: {listing}"
    );
    gc(&fixture);
    assert!(artifact.exists(), "active nested output removed");
    drop(lock);
    gc(&fixture);
    assert!(!directory.exists(), "completed nested cache retained");
}

#[test]
fn gc_preserves_out_dir_assets_during_nested_cargo_test_runtime() {
    runtime_gc_scenario(false, false);
}

#[test]
fn another_project_proceeds_while_nested_test_runtime_holds_its_cache() {
    runtime_gc_scenario(false, true);
}

#[test]
fn gc_preserves_runtime_assets_when_only_the_daemon_is_killed() {
    runtime_gc_scenario(true, false);
}

fn runtime_gc_scenario(kill_only_daemon: bool, check_other_project: bool) {
    let (fixture, store) = configured();
    let ready = fixture.path("runtime-ready");
    let gate = fixture.path("runtime-gate");
    let done = fixture.path("runtime-done");
    #[cfg(windows)]
    {
        // A supported target runner gives Windows the same resource-lifetime
        // test as Unix: its mapped executable lives outside the cache, so an
        // incidental Windows rename failure cannot protect OUT_DIR for us.
        let runner = fixture.path("runtime-runner.ps1");
        std::fs::write(
            &runner,
            r#"param([Parameter(ValueFromRemainingArguments=$true)][string[]]$CargoTestArgs)
$ErrorActionPreference = 'Stop'
$destination = Join-Path $env:HELI_FIXTURE_RUNTIME_BIN ([IO.Path]::GetFileName($CargoTestArgs[0]))
Copy-Item -LiteralPath $CargoTestArgs[0] -Destination $destination -Force
$remaining = @($CargoTestArgs | Select-Object -Skip 1)
& $destination @remaining
exit $LASTEXITCODE
"#,
        )
        .expect("write the Cargo target runner");
        let cargo = fixture.workspace().join(".cargo");
        std::fs::create_dir_all(&cargo).expect("Cargo configuration directory");
        std::fs::write(cargo.join("config.toml"), format!(
            "[target.x86_64-pc-windows-msvc]\nrunner = ['powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', '{}']\n",
            runner.display(),
        )).expect("configure the supported target runner");
    }
    let runtime_bin = fixture.path("runtime-bin");
    std::fs::create_dir_all(&runtime_bin).expect("runtime executable directory");
    let mut child = fixture
        .heli(["--no-cache", "test", "--quiet", "-p", "nested"])
        .env("CARGO_BUILD_BUILD_DIR", store.join("{workspace-path-hash}"))
        .env("CARGO_INCREMENTAL", "0")
        .env("HELI_FIXTURE_ARTIFACT_MB", "64")
        .env("HELI_FIXTURE_RUNTIME_READY", &ready)
        .env("HELI_FIXTURE_RUNTIME_GATE", &gate)
        .env("HELI_FIXTURE_RUNTIME_DONE", &done)
        .env("HELI_FIXTURE_RUNTIME_BIN", &runtime_bin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start nested Cargo runtime workload");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let asset = loop {
        // The file is made before its content is in it: empty, not ready.
        if let Ok(path) = std::fs::read_to_string(&ready)
            && !path.is_empty()
        {
            break std::path::PathBuf::from(path);
        }
        if child
            .try_wait()
            .expect("inspect runtime workload")
            .is_some()
        {
            let output = child.wait_with_output().expect("collect early failure");
            assert_success(&output, "runtime workload before gate");
            panic!("runtime workload exited without reaching its gate");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nested test did not reach runtime gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // The ready file is written by the actual compiled test, not build.rs.
    assert!(
        asset.starts_with(&store),
        "OUT_DIR must be in the shared store: {}",
        asset.display()
    );
    let mut other_project = None;
    if check_other_project {
        let other = fixture.copy_workspace("second-runtime-project");
        other_project = Some(other.clone());
        let mut second = fixture
            .heli(["--no-cache", "check", "--quiet", "-p", "echo-args"])
            .current_dir(&other)
            .env_remove("CARGO_TARGET_DIR")
            .env("CARGO_BUILD_BUILD_DIR", store.join("{workspace-path-hash}"))
            .env("CARGO_INCREMENTAL", "0")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("start a command in an independent workspace");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while second
            .try_wait()
            .expect("inspect independent command")
            .is_none()
        {
            if std::time::Instant::now() >= deadline {
                support::kill_tree(second.id());
                std::fs::write(&gate, "continue").expect("release first runtime");
                panic!("an independent workspace queued behind the active runtime lease");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let second = second
            .wait_with_output()
            .expect("collect independent command");
        assert_success(&second, "independent command while first runtime is gated");
        assert!(!done.exists(), "the first runtime must still be gated");
    }
    if kill_only_daemon {
        let pid = fixture.daemon_pid().expect("a real fixture daemon");
        // Fixture::kill_daemon intentionally kills the tree; this case must
        // kill exactly the daemon, leaving its actual Cargo/test descendants.
        support::kill_process(pid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while support::process_runs(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture daemon did not stop"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(asset.exists(), "runtime asset exists after daemon death");
        assert!(!done.exists(), "runtime is still gated after daemon death");
    }
    // An idle build directory, untouched since long before the run in
    // flight started, of no workspace a run is in: the run holds back only
    // what it may use, so this one goes over the budget meanwhile (#108).
    let idle = store.join("ab").join("0123456789abcd");
    if other_project.is_some() {
        let profile = idle.join("debug");
        std::fs::create_dir_all(profile.join("deps")).expect("an idle build directory");
        std::fs::File::create(profile.join(".cargo-lock"))
            .and_then(|lock| {
                lock.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            })
            .expect("cargo's lock, free and old");
        let data = std::fs::File::create(profile.join("deps").join("old-data")).expect("old data");
        data.set_len(1 << 20).expect("fill it");
        data.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .expect("date it long ago");
    }
    let eviction = gc(&fixture);
    assert_eq!(eviction["runtime_busy"], true, "{eviction}");
    if other_project.is_some() {
        assert!(
            !idle.exists(),
            "an idle directory is evicted while a run is in flight elsewhere: {eviction}"
        );
        assert!(asset.exists(), "the run's own asset stays");
    }
    let survived = asset.exists();
    std::fs::write(&gate, "continue").expect("release runtime workload");
    let output = child.wait_with_output().expect("collect runtime workload");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        survived && done.exists() && (kill_only_daemon || output.status.success()),
        "GC must preserve OUT_DIR assets until the real nested test ends; asset={}; existed_after_gc={survived}; eviction={eviction}; stdout={}; stderr={}",
        asset.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // A detached runtime can finish just after the disconnected client. Give
    // its guardian time to observe the end and release the activity lease.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let listing = loop {
        gc(&fixture);
        let listing = survey(&fixture);
        if listing["used"].as_u64().expect("accounted bytes") <= 16 << 20
            || std::time::Instant::now() >= deadline
        {
            break listing;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(
        listing["used"].as_u64().expect("accounted bytes") <= 16 << 20,
        "the runtime lease must release its cache after the nested test ends: {listing}",
    );
}
