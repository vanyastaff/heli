//! `cargo heli self-upgrade` (docs/daemon.md, "Upgrading"): installs a newer
//! cargo-heli beside the running one, swaps it in by renaming, and replaces
//! the daemon. The program upgraded is a copy of cargo-heli in the fixture,
//! and the fake cargo "installs" another copy of it.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use support::{Fixture, assert_success, copy_program, heli_binary, process_runs};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A copy of cargo-heli in the fixture's `dir`, so that the test binary
/// itself is never renamed.
fn copy_of_heli(fixture: &Fixture, dir: &str) -> PathBuf {
    let dir = fixture.path(dir);
    std::fs::create_dir_all(&dir).expect("create the directory");
    let program = dir.join(format!("cargo-heli{}", std::env::consts::EXE_SUFFIX));
    copy_program(&heli_binary(), &program);
    program
}

/// `echo-args` as `cargo`: its `install --root <dir>` puts the file
/// `ECHO_INSTALL_FROM` names in `<dir>/bin`.
fn fake_cargo(fixture: &Fixture) -> PathBuf {
    let dir = fixture.path("fake");
    std::fs::create_dir_all(&dir).expect("create the fake cargo's directory");
    let cargo = dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    copy_program(&fixture.echo_args(), &cargo);
    cargo
}

/// Starts a daemon of `program` with a run that ends at once; its pid.
fn daemon_of(fixture: &Fixture, program: &Path) -> u32 {
    let output = fixture
        .command(program, ["exec", "--"])
        .arg(fixture.echo_args())
        .stdin(Stdio::null())
        .output()
        .expect("run cargo-heli");
    assert_success(&output, "cargo heli exec");
    fixture.daemon_pid().expect("a daemon")
}

/// `<program> self-upgrade <args>`, with the fake cargo installing `newer`.
fn self_upgrade(
    fixture: &Fixture,
    program: &Path,
    newer: &Path,
    args: &[&str],
    configure: impl FnOnce(&mut Command),
) -> Output {
    let cargo = fake_cargo(fixture);
    let mut command = fixture.command(program, ["self-upgrade"].iter().chain(args));
    command
        .env("CARGO", &cargo)
        .env("ECHO_INSTALL_FROM", newer)
        .stdin(Stdio::null());
    configure(&mut command);
    command.output().expect("run cargo-heli self-upgrade")
}

/// The files beside `program` that an upgrade put aside.
fn aside(program: &Path) -> Vec<String> {
    let name = program
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned();
    let mut found: Vec<String> = std::fs::read_dir(program.parent().expect("a directory"))
        .expect("read the directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|file| file.starts_with(&format!("{name}.old-")))
        .collect();
    found.sort();
    found
}

fn version_of(program: &Path) -> String {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("run the program");
    assert_success(&output, "--version");
    text(&output.stdout).trim().to_owned()
}

/// The new program takes the old one's name and place, the old one is
/// put aside on Windows (a running image can only be renamed) and is gone
/// elsewhere, the install root is emptied, and the daemon is replaced by
/// one of the new program.
#[test]
fn self_upgrade_installs_beside_swaps_in_and_restarts() {
    let fixture = Fixture::new("basic");
    let program = copy_of_heli(&fixture, "bin");
    let newer = copy_of_heli(&fixture, "newer");
    let first = daemon_of(&fixture, &program);

    let output = self_upgrade(&fixture, &program, &newer, &["--to", "9.9.9"], |_| {});
    assert_success(&output, "cargo heli self-upgrade");
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);
    for arg in [
        "install",
        "cargo-heli",
        "--version",
        "9.9.9",
        "--locked",
        "--force",
        "--root",
    ] {
        assert!(stdout.contains(&format!("arg: {arg:?}")), "{arg}: {stdout}");
    }
    let root = fixture.state_dir().join("upgrade");
    assert!(
        stdout.contains(&format!("arg: {:?}", root.as_os_str())),
        "{stdout}"
    );
    assert!(
        !root
            .join("bin")
            .join(program.file_name().expect("a name"))
            .exists(),
        "the install root's copy is gone"
    );
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        stderr.contains(&format!("is now {version} (was {version})")),
        "{stderr}"
    );
    assert_eq!(version_of(&program), format!("cargo-heli {version}"));
    if cfg!(windows) {
        assert_eq!(aside(&program), [format!("cargo-heli.exe.old-{version}")]);
    } else {
        assert_eq!(aside(&program), Vec::<String>::new());
    }
    let second = fixture.daemon_pid().expect("a new daemon");
    assert_ne!(second, first, "{stderr}");
    assert!(
        stderr.contains(&format!("pid {second}, version {version}")),
        "{stderr}"
    );

    // The next upgrade deletes what was put aside, once nothing runs it:
    // here, once the old daemon has exited.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while process_runs(first) {
        assert!(
            std::time::Instant::now() < deadline,
            "the old daemon {first} did not exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = self_upgrade(&fixture, &program, &newer, &["--path", "."], |_| {});
    assert_success(&output, "cargo heli self-upgrade --path .");
    let stdout = text(&output.stdout);
    assert!(stdout.contains("arg: \"--path\""), "{stdout}");
    assert!(!stdout.contains("arg: \"cargo-heli\""), "{stdout}");
    if cfg!(windows) {
        assert_eq!(aside(&program), [format!("cargo-heli.exe.old-{version}")]);
    } else {
        assert_eq!(aside(&program), Vec::<String>::new());
    }
}

/// A failed install changes nothing: the program and its daemon stay.
#[test]
fn a_failed_install_leaves_the_program_and_the_daemon() {
    let fixture = Fixture::new("basic");
    let program = copy_of_heli(&fixture, "bin");
    let newer = copy_of_heli(&fixture, "newer");
    let first = daemon_of(&fixture, &program);
    let before = std::fs::metadata(&program).expect("the program");

    let output = self_upgrade(&fixture, &program, &newer, &[], |command| {
        command.env("ECHO_EXIT", "1");
    });
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("cargo install exited") && stderr.contains("is unchanged"),
        "{stderr}"
    );
    let after = std::fs::metadata(&program).expect("the program");
    assert_eq!(
        after.modified().expect("mtime"),
        before.modified().expect("mtime")
    );
    assert_eq!(aside(&program), Vec::<String>::new());
    assert_eq!(fixture.daemon_pid(), Some(first));
}
