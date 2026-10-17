//! Survivors and direct-in-pool runs (`DESIGN.md` §5, "Tool profiles").

mod support;

use support::{Fixture, assert_success};

/// A run whose `RUSTC_WRAPPER` is sccache has the daemon start the compiler
/// cache's server first, outside the run's process tree.
#[test]
fn the_daemon_starts_the_sccache_server_for_a_run() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let bin = fixture.path("bin");
    std::fs::create_dir_all(&bin).expect("create bin");
    let sccache = bin.join(format!("sccache{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&echo, &sccache).expect("copy echo-args as sccache");
    let starts = fixture.path("starts");

    let output = fixture
        .heli(["xtask", "go"])
        .env("CARGO", &echo)
        .env("RUSTC_WRAPPER", &sccache)
        .env("ECHO_STARTS", &starts)
        .env("HELI_OUTPUT", "full")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run heli");
    assert_success(&output, "a run with an sccache wrapper");
    // Once for the fake sccache, once for the run's own cargo.
    let started = std::fs::read_to_string(&starts).expect("starts");
    assert_eq!(started.lines().count(), 2, "{started}");
}

/// An interactive tool runs in the client, not the daemon, but draws from
/// the host pool: it gets the pool's `CARGO_MAKEFLAGS`.
#[test]
fn an_interactive_tool_runs_directly_in_the_pool() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["insta", "review"])
        .env("CARGO", &echo)
        .env("ECHO_ENV", "CARGO_MAKEFLAGS")
        .env("HELI_OUTPUT", "full")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run heli");
    assert_success(&output, "insta review");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("arg: \"review\""), "{text}");
    assert!(text.contains("--jobserver-auth="), "{text}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("running the command directly"),
        "no fallback warning"
    );
}
