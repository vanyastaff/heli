//! Tool profiles from configuration (`DESIGN.md` §5, "Tool profiles"): the
//! user's give a tool its class, and a repository's only narrow it.

mod support;

use std::collections::BTreeMap;
use std::process::{Child, Stdio};

use support::{Fixture, assert_success, runs};

#[test]
fn user_and_repository_profiles_give_runs_their_class() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    fixture.configure(
        "[[profiles.tool]]\ntool = \"xtask\"\nargs = [\"check-changed\"]\nclass = \"verify\"\n",
    );
    let config = fixture.workspace().join(".config");
    std::fs::create_dir_all(&config).expect("create .config");
    std::fs::write(
        config.join("heli.toml"),
        "[[profiles.tool]]\ntool = \"xtask\"\nargs = [\"gen\"]\nclass = \"mutate\"\n\
         [[profiles.tool]]\ntool = \"xtask\"\nargs = [\"lint\"]\nclass = \"verify\"\n",
    )
    .expect("write the repository's profiles");

    let gate = fixture.path("gate");
    let start = |args: &[&str]| -> Child {
        fixture
            .heli(args.iter().copied())
            .env("CARGO", &echo)
            .env("ECHO_WAIT_FOR", &gate)
            .env("HELI_OUTPUT", "full")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start heli")
    };
    // `gen` is mutate and waits for the others' shared locks; start it last.
    let mut calls = vec![
        start(&["xtask", "check-changed"]),
        start(&["xtask", "lint"]),
    ];
    fixture.wait_for_status("both runs to be listed", |status| runs(status).len() == 2);
    calls.push(start(&["xtask", "gen"]));
    let status = fixture.wait_for_status("every run to be listed", |status| {
        runs(status).len() == calls.len()
    });
    let classes: BTreeMap<String, String> = runs(&status)
        .iter()
        .map(|run| {
            let args: Vec<&str> = run["argv"].as_array().expect("an argv")[1..]
                .iter()
                .map(|arg| arg.as_str().expect("a string"))
                .collect();
            (
                args.join(" "),
                run["class"].as_str().expect("a class").to_owned(),
            )
        })
        .collect();
    std::fs::write(&gate, "").expect("open the gate");
    for call in calls {
        let output = call.wait_with_output().expect("wait for heli");
        assert_success(&output, "a profiled run");
    }
    let expected: BTreeMap<String, String> = [
        ("xtask check-changed", "verify"),
        // The repository may take the lock, not make a tool cacheable.
        ("xtask gen", "mutate"),
        ("xtask lint", "exec"),
    ]
    .into_iter()
    .map(|(args, class)| (args.to_owned(), class.to_owned()))
    .collect();
    assert_eq!(classes, expected);
}

#[test]
fn an_invalid_user_profile_is_refused() {
    let fixture = Fixture::new("basic");
    fixture.configure("[[profiles.tool]]\ntool = \"xtask\"\nclass = \"test\"\n");
    let output = fixture
        .heli(["xtask", "go"])
        .stdin(Stdio::null())
        .output()
        .expect("run heli");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("[profiles]"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
