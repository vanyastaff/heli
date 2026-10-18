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

/// The same for a wrapper the Cargo configuration sets, not a variable.
#[test]
fn a_configured_sccache_wrapper_is_started_too() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let bin = fixture.path("bin");
    std::fs::create_dir_all(&bin).expect("create bin");
    let sccache = bin.join(format!("sccache{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&echo, &sccache).expect("copy echo-args as sccache");
    let config = fixture.workspace().join(".cargo");
    std::fs::create_dir_all(&config).expect("create .cargo");
    std::fs::write(
        config.join("config.toml"),
        format!("[build]\nrustc-wrapper = '{}'\n", sccache.display()),
    )
    .expect("write the configuration");
    let starts = fixture.path("starts");

    let output = fixture
        .heli(["xtask", "go"])
        .env("CARGO", &echo)
        .env("ECHO_STARTS", &starts)
        .env("HELI_OUTPUT", "full")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run heli");
    assert_success(&output, "a run with a configured sccache wrapper");
    let started = std::fs::read_to_string(&starts).expect("starts");
    assert_eq!(started.lines().count(), 2, "{started}");
}

/// At a terminal, a tool no profile names runs in the client, with the host
/// pool: it gets the pool's `CARGO_MAKEFLAGS`, and not `HELI_RUN`, which only
/// a run the daemon executes has (#64).
#[test]
fn an_unknown_tool_at_a_terminal_runs_directly_in_the_pool() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let shown = |variable: &str| {
        let mut command = fixture.heli(["bloat", "--release"]);
        command
            .env("CARGO", &echo)
            .env("ECHO_ENV", variable)
            .env("HELI_OUTPUT", "full");
        let shown = support::terminal::run(&mut command);
        assert_eq!(shown.code, Some(0), "{}", shown.text);
        shown.text
    };
    let makeflags = shown("CARGO_MAKEFLAGS");
    assert!(makeflags.contains("--jobserver-auth="), "{makeflags}");
    let run = shown("HELI_RUN");
    assert!(run.contains("HELI_RUN=None"), "{run}");
    assert!(
        !run.contains("running the command directly"),
        "no fallback warning: {run}"
    );
}
