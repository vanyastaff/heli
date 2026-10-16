//! Builds the `inner` project with a nested cargo, as a test that generates
//! and builds a project would.

use std::process::Command;

#[test]
fn builds_inner_project() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let subcommand = if std::env::var_os("HELI_FIXTURE_RUNTIME_READY").is_some() {
        "test"
    } else {
        "build"
    };
    let status = Command::new(cargo)
        .args([subcommand, "--quiet", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/inner/Cargo.toml"))
        .env(
            "CARGO_TARGET_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/inner"),
        )
        .status()
        .expect("run the nested cargo");
    assert!(status.success(), "nested cargo build: {status}");
}
