//! The endpoint's owner (docs/daemon.md, "The endpoint"): on Windows the
//! daemon's pipe is owned by the user's SID, elevated or not, and a client
//! talks only to a pipe owned by its own user's SID: not to one owned by
//! the Administrators group, which any elevated process of an administrator
//! can create.

#![cfg(windows)]

mod support;

use support::endpoint;
use support::{Fixture, assert_success, lines};

/// The daemon's pipe is owned by the user's SID, not by the default owner
/// of what the daemon creates (the Administrators group when elevated).
#[test]
fn the_daemons_pipe_is_owned_by_the_user() {
    let fixture = Fixture::new("basic");
    fixture.start_daemon();
    assert_eq!(
        endpoint::owner(&fixture.endpoint()),
        endpoint::current_user_sid()
    );
}

/// A pipe at the endpoint owned by the Administrators group is not the
/// user's daemon: the client does not talk to it, and runs the command
/// directly after one warning that says why. Only an elevated process can
/// give a pipe that owner, so elsewhere this checks nothing.
#[test]
fn a_pipe_owned_by_the_administrators_is_not_trusted() {
    let fixture = Fixture::new("basic");
    let Some(_pipe) = endpoint::pipe_owned_by(&fixture.endpoint(), "BA") else {
        eprintln!("not elevated: no pipe owned by the Administrators group can be made");
        return;
    };
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("direct")
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    let stderr = lines(&output.stderr);
    assert!(
        stderr.len() == 2
            && stderr[0].starts_with("cargo-heli: warning: ")
            && stderr[0].contains("another user")
            && stderr[0].ends_with("; running the command directly"),
        "{stderr:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "arg: \"direct\"\n",
        "the command ran directly"
    );
}
