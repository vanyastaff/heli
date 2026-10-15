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

#[test]
fn nested_cargo_artifacts_can_be_reclaimed_under_the_shared_budget() {
    let (fixture, store) = configured();
    nested_build(&fixture, &store);
    let listing = gc(&fixture);
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
    let listing = gc(&fixture);
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
