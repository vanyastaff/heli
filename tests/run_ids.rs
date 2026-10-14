//! Run ids are unique for the life of the state directory: every daemon
//! that uses it (on Windows, one per logon session) numbers its runs from
//! the counter there, and an id whose record exists is never given again,
//! even when the counter is lost.

mod support;

use support::{Fixture, assert_success};

/// Runs `echo-args` with `arg` through heli, and returns the run its footer
/// names.
fn run(fixture: &Fixture, arg: &str) -> String {
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg(arg)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    support::split_footer(&output.stderr).1.run
}

/// What `cargo heli log <run>` prints on stdout.
fn logged(fixture: &Fixture, run: &str) -> String {
    let output = fixture.heli(["log", run]).output().expect("run heli log");
    assert_success(&output, &format!("cargo heli log {run}"));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The counter of run numbers in the state directory.
fn counter(fixture: &Fixture) -> std::path::PathBuf {
    fixture.state_dir().join("next-run")
}

/// A daemon takes each id from the counter in the state directory as it
/// numbers a run, not from a copy of its own: here another daemon of the
/// same state directory (as in another logon session) has numbered runs up
/// to r99 meanwhile.
#[test]
fn each_id_comes_from_the_state_directory() {
    let fixture = Fixture::new("basic");
    assert_eq!(run(&fixture, "first"), "r1");
    std::fs::write(counter(&fixture), "100").expect("write the counter");
    assert_eq!(run(&fixture, "second"), "r100");
    assert_eq!(run(&fixture, "third"), "r101");
}

/// A lost counter does not start the ids again at r1: the next id is above
/// every record in the run log, and the records already there stay as they
/// were.
#[test]
fn a_lost_counter_does_not_reuse_ids() {
    let fixture = Fixture::new("basic");
    assert_eq!(run(&fixture, "first"), "r1");
    assert_eq!(run(&fixture, "second"), "r2");
    fixture.kill_daemon();
    std::fs::remove_file(counter(&fixture)).expect("lose the counter");

    assert_eq!(run(&fixture, "third"), "r3");
    assert_eq!(logged(&fixture, "r1"), "arg: \"first\"\n");
    assert_eq!(logged(&fixture, "r2"), "arg: \"second\"\n");
}

/// A number that cannot be written back to the counter is not given: the
/// daemon refuses the run, and the client runs the command directly after
/// one warning.
#[test]
fn a_number_that_cannot_be_kept_is_not_given() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    std::fs::create_dir_all(counter(&fixture)).expect("make the counter a directory");
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .arg("direct")
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "arg: \"direct\"\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (warning, rest) = stderr.split_once('\n').expect("a warning line");
    assert!(
        warning.starts_with("cargo-heli: warning: the heli daemon cannot number the run: ")
            && warning.ends_with("; running the command directly"),
        "{stderr}"
    );
    assert_eq!(rest, "echo-args: stderr\n", "no footer: {stderr}");
}

/// A counter behind the run log (another daemon wrote records past it, or
/// it was restored from an old copy) is overtaken: no record is written
/// over.
#[test]
fn an_id_with_a_record_is_never_given_again() {
    let fixture = Fixture::new("basic");
    assert_eq!(run(&fixture, "first"), "r1");
    assert_eq!(run(&fixture, "second"), "r2");
    fixture.kill_daemon();
    std::fs::write(counter(&fixture), "1").expect("set the counter back");

    assert_eq!(run(&fixture, "third"), "r3");
    assert_eq!(logged(&fixture, "r1"), "arg: \"first\"\n");
    assert_eq!(logged(&fixture, "r2"), "arg: \"second\"\n");
}
