//! `cargo heli` as a drop-in: it runs cargo, or any command, with arguments,
//! stdio and exit code unchanged. These calls go through the fixture's
//! daemon; `tests/daemon.rs` covers the daemon itself.

mod support;

use std::ffi::OsString;

use support::{Fixture, assert_success};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The `arg: …` lines `echo-args` prints for `args`.
fn echoed(args: &[&str]) -> String {
    args.iter().map(|arg| format!("arg: {arg:?}\n")).collect()
}

#[test]
fn a_cargo_subcommand_builds_like_cargo() {
    let fixture = Fixture::new("basic");
    let heli = fixture
        .heli(["build", "-p", "slow-a"])
        .output()
        .expect("run heli");
    assert_success(&heli, "cargo heli build");
    let rlib = fixture.target_dir().join("debug").join("libslow_a.rlib");
    assert!(rlib.is_file(), "{} was not built", rlib.display());
}

#[test]
fn a_failing_build_exits_with_cargos_code() {
    let fixture = Fixture::new("basic");
    let heli = fixture
        .heli(["build", "-p", "broken"])
        .output()
        .expect("run heli");
    let cargo = fixture
        .cargo(["build", "-p", "broken"])
        .output()
        .expect("run cargo");
    assert!(!cargo.status.success(), "the fixture must not compile");
    assert_eq!(heli.status.code(), cargo.status.code());
    assert!(
        text(&heli.stderr).contains("error[E0308]"),
        "cargo's diagnostics reach the caller: {}",
        text(&heli.stderr)
    );
}

#[test]
fn exec_passes_arguments_through_verbatim() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let args = [
        "--output",
        "quiet",
        "-q",
        "--no-cache",
        "two words",
        "",
        "--",
        "exec",
    ];
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .args(args)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    assert_eq!(text(&output.stdout), echoed(&args));
}

#[test]
fn exec_exits_with_the_commands_code() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    for code in [0, 3, 101] {
        let output = fixture
            .heli(["exec", "--"])
            .arg(&echo)
            .env("ECHO_EXIT", code.to_string())
            .output()
            .expect("run heli");
        assert_eq!(output.status.code(), Some(code), "ECHO_EXIT={code}");
    }
}

#[test]
fn stdout_and_stderr_stay_separate() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("out")
        .output()
        .expect("run heli");
    assert_eq!(text(&output.stdout), echoed(&["out"]));
    assert_eq!(
        support::without_footer(&output.stderr).trim(),
        "echo-args: stderr"
    );
}

#[test]
fn a_flag_before_the_subcommand_is_heli_s_and_after_it_the_tool_s() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();

    for args in [
        &["--bogus", "build"][..],
        &["exec", "--bogus", "--", "x"][..],
    ] {
        let output = fixture
            .heli(args.iter().copied())
            .output()
            .expect("run heli");
        assert_eq!(output.status.code(), Some(2), "usage error for {args:?}");
        assert!(
            text(&output.stderr).contains("--bogus"),
            "the error names the flag: {}",
            text(&output.stderr)
        );
    }

    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("--bogus")
        .output()
        .expect("run heli");
    assert_eq!(text(&output.stdout), echoed(&["--bogus"]));

    let heli = fixture
        .heli(["build", "--bogus"])
        .output()
        .expect("run heli");
    let cargo = fixture
        .cargo(["build", "--bogus"])
        .output()
        .expect("run cargo");
    assert_eq!(heli.status.code(), cargo.status.code());
    assert_eq!(
        support::without_footer(&heli.stderr),
        text(&cargo.stderr),
        "cargo itself rejects it"
    );
}

#[test]
fn heli_without_a_subcommand_prints_usage() {
    let fixture = Fixture::new("basic");
    let output = fixture
        .heli(Vec::<OsString>::new())
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("Usage: cargo heli"));
}

#[test]
fn a_missing_command_exits_127() {
    let fixture = Fixture::new("basic");
    let output = fixture
        .heli(["exec", "--", "heli-test-no-such-command"])
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(127));
    assert!(text(&output.stderr).contains("cannot run `heli-test-no-such-command`"));
}

/// Through cargo itself: `cargo heli` finds `cargo-heli` on the search path
/// and passes `heli` as its first argument.
#[test]
fn runs_as_a_cargo_subcommand() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let bin_dir = support::heli_binary()
        .parent()
        .expect("the binary's directory")
        .to_path_buf();
    let path = std::env::join_paths(std::iter::once(bin_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .expect("a search path");
    let output = fixture
        .cargo(["heli", "exec", "--"])
        .arg(&echo)
        .arg("via cargo")
        .env("PATH", path)
        .output()
        .expect("run cargo heli");
    assert_success(&output, "cargo heli exec");
    assert_eq!(text(&output.stdout), echoed(&["via cargo"]));
}

#[test]
fn the_concurrency_log_records_every_build_script() {
    let fixture = Fixture::new("basic");
    let log = fixture.concurrency_log();
    let mut build = fixture.heli([
        "build", "-p", "slow-a", "-p", "slow-b", "-p", "slow-c", "-j", "1",
    ]);
    log.attach(&mut build, 50);
    assert_success(&build.output().expect("run heli"), "cargo heli build");
    let mut names: Vec<String> = log.intervals().into_iter().map(|run| run.name).collect();
    names.sort();
    assert_eq!(names, ["slow-a", "slow-b", "slow-c"]);
    assert!(log.intervals().iter().all(|run| run.end >= run.start));
    assert_eq!(
        log.max_concurrency(),
        1,
        "`-j 1` runs one build script at a time"
    );
}

/// `cargo heli heli …` would make heli run itself; it is refused up front.
#[test]
fn heli_is_not_a_subcommand_of_itself() {
    let fixture = Fixture::new("basic");
    // The first `heli` is the one cargo passes; the second is the subcommand.
    let output = fixture
        .heli(["heli", "heli", "exec", "--", "x"])
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("would run heli again"),
        "{}",
        text(&output.stderr)
    );
}

/// Each command heli runs sees `HELI_DEPTH` one deeper, and heli refuses to
/// start once the depth reaches the limit, so a loop that runs heli again
/// stops after a bounded number of processes.
#[test]
fn nesting_depth_is_passed_down_and_bounded() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let depth_seen = |depth: Option<&str>| {
        let mut command = fixture.heli(["exec", "--"]);
        command.arg(&echo).env("ECHO_ENV", "HELI_DEPTH");
        match depth {
            Some(depth) => command.env("HELI_DEPTH", depth),
            None => command.env_remove("HELI_DEPTH"),
        };
        command.output().expect("run heli")
    };
    for (given, child) in [(None, "1"), (Some("3"), "4"), (Some("garbage"), "1")] {
        let output = depth_seen(given);
        assert_success(&output, "cargo heli exec");
        assert_eq!(
            text(&output.stdout),
            format!("env HELI_DEPTH=Some({child:?})\n"),
            "HELI_DEPTH={given:?}"
        );
    }
    let output = depth_seen(Some("16"));
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("nested 16 levels deep"));
    assert!(text(&output.stdout).is_empty(), "the command must not run");
}
