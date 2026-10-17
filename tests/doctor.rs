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
