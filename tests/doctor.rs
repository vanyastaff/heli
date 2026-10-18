//! `cargo heli doctor`: explains the cargo configuration that reaches the
//! workspace, without a daemon.

mod support;

use serde_json::Value;
use support::{Fixture, assert_success};

#[test]
fn doctor_explains_the_configuration_of_the_workspace() {
    let fixture = Fixture::new("basic");
    let dir = fixture.workspace().join(".cargo");
    std::fs::create_dir_all(&dir).expect("create .cargo");
    std::fs::write(
        dir.join("config.toml"),
        "[build]\njobs = 16\nbuild-dir = \"E:/mine\"\n",
    )
    .expect("write the configuration");

    let output = fixture
        .heli(["doctor", "--json"])
        .output()
        .expect("run heli doctor");
    assert_success(&output, "cargo heli doctor --json");
    let report: Value = serde_json::from_slice(&output.stdout).expect("a JSON report");
    assert_eq!(report["schema"], 1, "{report}");
    let ids: Vec<&str> = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|finding| finding["id"].as_str().expect("an id"))
        .collect();
    // The fixture's own CARGO_TARGET_DIR is a global target directory.
    for id in ["jobs", "build-dir", "target-dir"] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }

    let output = fixture.heli(["doctor"]).output().expect("run heli doctor");
    assert_success(&output, "cargo heli doctor");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("has no effect"), "{text}");
}

/// Runs of tools no profile names are remembered, with their profile, and
/// doctor lists them by use.
#[test]
fn doctor_lists_the_unknown_tools_used_most() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    for args in [
        &["bloat"][..],
        &["bloat", "--release"],
        &["expand"],
        &["build"],
    ] {
        let output = fixture
            .heli(args.iter().copied())
            .env("CARGO", &echo)
            .env("HELI_OUTPUT", "full")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run heli");
        assert_success(&output, "a run");
    }
    let output = fixture
        .heli(["doctor", "--json"])
        .output()
        .expect("run heli doctor");
    assert_success(&output, "cargo heli doctor --json");
    let report: Value = serde_json::from_slice(&output.stdout).expect("a JSON report");
    assert_eq!(
        report["unknown_tools"],
        serde_json::json!([{"name": "bloat", "runs": 2}, {"name": "expand", "runs": 1}]),
        "{report}"
    );
}

/// A `CARGO_MAKEFLAGS` naming another jobserver than this host pool, as one
/// written before the state directory moved does, is a warning with the
/// value setup would write now (#86); this pool's own is not.
#[test]
fn doctor_reports_makeflags_for_another_pool() {
    let fixture = Fixture::new("basic");
    let doctor = |makeflags: &str| {
        let output = fixture
            .heli(["doctor", "--json"])
            .env("CARGO_MAKEFLAGS", makeflags)
            .output()
            .expect("run heli doctor");
        assert_success(&output, "cargo heli doctor --json");
        let report: Value = serde_json::from_slice(&output.stdout).expect("a JSON report");
        report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .find(|finding| finding["id"] == "makeflags")
            .cloned()
    };
    let stale = fixture.makeflags().replace("heli", "heli-moved");
    let finding = doctor(&stale).expect("a makeflags finding");
    assert_eq!(finding["level"], "warn", "{finding}");
    let action = finding["action"].as_str().expect("an action");
    assert!(action.contains(&fixture.makeflags()), "{finding}");
    assert_eq!(doctor(&fixture.makeflags()), None);
}
