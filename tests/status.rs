//! `cargo heli status` describes the daemon (docs/observability.md): where it
//! runs, so that a user sees it runs in their interactive session, which tests
//! that drive the desktop need (docs/daemon.md).

mod support;

use support::{Fixture, assert_success};

/// What `echo-args` says of where it runs, run directly by the test, as
/// the client that started the daemon would: on Windows `<session>
/// <window station>\<desktop>`, elsewhere `<session>`.
fn where_the_test_runs(fixture: &Fixture) -> String {
    let output = fixture
        .command(fixture.echo_args(), Vec::<String>::new())
        .env("ECHO_SESSION", "1")
        .output()
        .expect("run echo-args");
    assert_success(&output, "echo-args");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("session ").map(str::to_owned))
        .expect("a session line")
}

/// The daemon's session, and on Windows its window station and desktop, in
/// `status --json` and in the text form: those of the user who started it
/// here, the interactive window station `WinSta0`. Elsewhere it has no
/// window station or desktop.
#[test]
fn status_shows_where_the_daemon_runs() {
    let fixture = Fixture::new("basic");
    let here = where_the_test_runs(&fixture);
    fixture.start_daemon();
    let status = fixture.status();
    let daemon = &status["daemon"];
    let session = daemon["session"].as_u64().expect("a session");
    if cfg!(windows) {
        let station = daemon["window_station"].as_str().expect("a window station");
        let desktop = daemon["desktop"].as_str().expect("a desktop");
        assert_eq!(format!("{session} {station}\\{desktop}"), here, "{daemon}");
        assert_eq!(
            daemon["interactive"],
            station.eq_ignore_ascii_case("WinSta0") && session != 0,
            "{daemon}"
        );
    } else {
        assert!(daemon["window_station"].is_null(), "{daemon}");
        assert!(daemon["desktop"].is_null(), "{daemon}");
        assert!(session > 0, "{daemon}");
    }

    let output = fixture.heli(["status"]).output().expect("run heli status");
    assert_success(&output, "cargo heli status");
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().expect("a daemon line");
    assert!(line.contains(&format!("session {session}")), "{line}");
    if cfg!(windows) {
        let place = format!(
            "{}\\{}",
            daemon["window_station"].as_str().unwrap_or_default(),
            daemon["desktop"].as_str().unwrap_or_default()
        );
        assert!(line.contains(&place), "{line}");
    }
}
