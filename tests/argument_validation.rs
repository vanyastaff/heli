//! Invalid runner options must retain the native failure and exit code.
mod support;

use support::{Fixture, assert_success};

#[test]
fn an_invalid_libtest_thread_count_is_not_replaced_with_a_valid_one() {
    let fixture = Fixture::new("basic");
    let args = ["test", "-p", "many-tests", "--", "--test-threads=0"];
    let direct = fixture.cargo(args).output().unwrap();
    assert!(
        !direct.status.success(),
        "invalid count must be rejected by the runner"
    );
    let heli = fixture.heli(args).output().unwrap();
    assert_eq!(
        heli.status.code(),
        direct.status.code(),
        "heli changed a failing invocation into success: {heli:?}"
    );
}

#[test]
fn an_invalid_nextest_thread_count_is_not_replaced_with_a_valid_one() {
    let fixture = Fixture::new("basic");
    let args = [
        "nextest",
        "run",
        "-p",
        "many-tests",
        "--test-threads=invalid",
    ];
    let direct = fixture.cargo(args).output().unwrap();
    assert!(
        !direct.status.success(),
        "invalid count must be rejected by nextest"
    );
    let heli = fixture.heli(args).output().unwrap();
    assert_eq!(
        heli.status.code(),
        direct.status.code(),
        "heli changed a failing invocation into success: {heli:?}"
    );
}

#[test]
fn an_invalid_test_thread_environment_is_not_hidden() {
    let fixture = Fixture::new("basic");
    let direct = fixture
        .cargo(["test", "-p", "many-tests"])
        .env("RUST_TEST_THREADS", "invalid")
        .output()
        .unwrap();
    assert!(
        !direct.status.success(),
        "invalid environment must be rejected by libtest"
    );
    let heli = fixture
        .heli(["test", "-p", "many-tests"])
        .env("RUST_TEST_THREADS", "invalid")
        .output()
        .unwrap();
    assert_eq!(
        heli.status.code(),
        direct.status.code(),
        "heli hid an invalid runner environment: {heli:?}"
    );
    let control = fixture.heli(["test", "-p", "many-tests"]).output().unwrap();
    assert_success(&control, "valid control invocation");
}

#[test]
fn a_missing_thread_count_is_not_replaced_with_a_valid_one() {
    let fixture = Fixture::new("basic");
    for args in [
        vec!["test", "-p", "many-tests", "--", "--test-threads"],
        vec!["nextest", "run", "-p", "many-tests", "--test-threads"],
        vec!["nextest", "run", "-p", "many-tests", "-j"],
    ] {
        let direct = fixture.cargo(&args).output().unwrap();
        assert!(
            !direct.status.success(),
            "native runner must reject {args:?}"
        );
        let heli = fixture.heli(&args).output().unwrap();
        assert_eq!(
            heli.status.code(),
            direct.status.code(),
            "{args:?}: {heli:?}"
        );
    }
}
