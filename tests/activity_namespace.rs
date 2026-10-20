//! Invalid namespace overrides must never bypass active runtime protection.
mod support;

use std::fs::File;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;
use support::{Fixture, assert_success};

#[test]
fn a_relative_activity_override_cannot_delete_runtime_assets() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("store");
    fixture.configure(&format!(
        "[store]\npaths = ['{}']\nbudget_mb = 1\nunit_hours = 0\n\
         orphan_hours = 0\nidle_hours = 0\ndir_hours = 0\nunknown_hours = 0\n",
        store.display()
    ));
    fixture.start_daemon();
    let directory = store.join("ab").join("1234");
    let profile = directory.join("debug");
    std::fs::create_dir_all(profile.join("deps")).unwrap();
    File::create(profile.join(".cargo-lock")).unwrap();
    let artifact = profile.join("deps/runtime-data");
    let file = File::create(&artifact).unwrap();
    file.set_len(2 << 20).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    drop(file);

    let activity = fixture.path("activity");
    std::fs::create_dir_all(&activity).unwrap();
    let guard = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(activity.join(".heli-runtime.lock"))
        .unwrap();
    guard.lock_shared().unwrap();
    // A client and its daemon resolve relative paths from different cwd's.
    // Rejecting this override must keep eviction fail-closed, rather than
    // silently looking at an unrelated, unlocked activity file.
    let output = fixture
        .heli(["gc", "--json"])
        .env("HELI_ACTIVITY_DIR", "relative-activity")
        .output()
        .unwrap();
    assert_success(&output, "GC with invalid activity namespace");
    let listing: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        artifact.exists(),
        "invalid override bypassed runtime guard: {listing}"
    );
    assert!(
        listing["victims"].as_array().unwrap().is_empty(),
        "{listing}"
    );
    assert!(
        !fixture.workspace().join("relative-activity").exists(),
        "invalid namespace created an unrelated lock directory"
    );

    drop(guard);
    let output = fixture.heli(["gc", "--json"]).output().unwrap();
    assert_success(&output, "GC in the correct namespace after runtime ends");
    assert!(
        !artifact.exists(),
        "control: fixture assets were not reclaimable: {output:?}"
    );
}
