//! Dependency ownership through the same CLI an agent uses.
mod support;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use support::{Fixture, assert_success};

struct ReleaseOnDrop(PathBuf);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "go");
    }
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}

/// An entirely offline Cargo directory source, including Cargo's checksums.
fn vendor(fixture: &Fixture, value: u32) -> (PathBuf, PathBuf) {
    let root = fixture.path(&format!("vendor-{value}"));
    let package = root.join("heli-epoch-dep");
    let manifest = "[package]\nname='heli-epoch-dep'\nversion='1.0.0'\nedition='2024'\n";
    let library = format!("pub const VALUE: u32 = {value};\n");
    write(&package.join("Cargo.toml"), manifest);
    write(&package.join("src/lib.rs"), &library);
    let checksum = serde_json::json!({
        "$comment": "Cargo vendor checksum metadata is informational, not an additional checked source file.",
        "files": {
            "Cargo.toml": format!("{:x}", Sha256::digest(manifest.as_bytes())),
            "src/lib.rs": format!("{:x}", Sha256::digest(library.as_bytes()))
        },
        "package": "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
    });
    write(&package.join(".cargo-checksum.json"), checksum.to_string());
    let config = fixture.path(&format!("vendor-{value}.toml"));
    write(
        &config,
        format!(
            "[source.crates-io]\nreplace-with='heli-epoch'\n[source.heli-epoch]\ndirectory={}\n",
            quoted(&root)
        ),
    );
    (root, config)
}

fn import(fixture: &Fixture, store: &Path, source: &(PathBuf, PathBuf)) -> PathBuf {
    let output = fixture
        .heli(["deps", "import", "--store"])
        .arg(store)
        .arg("--vendor")
        .arg(&source.0)
        .arg("--config")
        .arg(&source.1)
        .output()
        .unwrap();
    assert_success(&output, "heli deps import");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn gc(fixture: &Fixture, store: &Path) -> serde_json::Value {
    let output = fixture
        .heli(["deps", "gc", "--store"])
        .arg(store)
        .output()
        .unwrap();
    assert_success(&output, "heli deps gc");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn wait_file(path: &Path, child: &mut std::process::Child) {
    let until = Instant::now() + Duration::from_secs(40);
    while !path.is_file() {
        if let Some(status) = child.try_wait().unwrap() {
            use std::io::Read;
            let mut diagnostics = String::new();
            if let Some(stderr) = child.stderr.as_mut() {
                let _ = stderr.read_to_string(&mut diagnostics);
            }
            let disk_log =
                std::fs::read_to_string(path.parent().unwrap().join("stderr")).unwrap_or_default();
            panic!(
                "Cargo exited {status} before {}: {diagnostics}\n{disk_log}",
                path.display()
            );
        }
        assert!(
            Instant::now() < until,
            "Cargo blocked before {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn project(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\n[package]\nname='epoch-consumer'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nheli-epoch-dep='=1.0.0'\n",
    );
    // Fixture temp roots can themselves live below Heli's checkout. Give
    // each consumer a real repository identity, as separate projects have.
    let git = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .output()
        .unwrap();
    assert_success(&git, "initialize independent consumer repository");
    write(
        &root.join("src/lib.rs"),
        "pub fn value() -> u32 { heli_epoch_dep::VALUE }\n#[test] fn reads_dependency() { assert_eq!(value(), 7); }\n",
    );
    write(
        &root.join("build.rs"),
        r#"
use std::{env, fs, path::Path, thread, time::Duration};
fn main() {
    let ready = env::var("EPOCH_READY").unwrap();
    let release = env::var("EPOCH_RELEASE").unwrap();
    fs::write(&ready, env::var("CARGO_HOME").unwrap()).unwrap();
    if let Ok(report) = env::var("EPOCH_ENV_REPORT") {
        fs::write(report, env::var("HELI_HOME_CANARY").unwrap()).unwrap();
    }
    while !Path::new(&release).is_file() { thread::sleep(Duration::from_millis(20)); }
    let source = env::var("EPOCH_SOURCE").unwrap();
    assert!(fs::read_to_string(source).unwrap().contains("VALUE: u32 = 7"));
    if let Ok(done) = env::var("EPOCH_DONE") { fs::write(done, "read after collection").unwrap(); }
}
"#,
    );
}

#[test]
fn independent_homes_and_live_epoch_survive_collection_of_another_epoch() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("dependency-store");
    let first_source = vendor(&fixture, 7);
    let selected = import(&fixture, &store, &first_source);
    let retired = import(&fixture, &store, &vendor(&fixture, 8));
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    write(
        &original.join("config.toml"),
        std::fs::read(&first_source.1).unwrap(),
    );
    let baseline_lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(original.join(".package-cache"))
        .unwrap();
    baseline_lock.lock().unwrap();
    let control_workspace = fixture.path("baseline-control");
    project(&control_workspace);
    let control_log = fixture.path("baseline-control.stderr");
    let mut control = fixture
        .cargo(["metadata", "--format-version", "1", "--offline"])
        .current_dir(&control_workspace)
        .env("CARGO_HOME", &original)
        .stdout(Stdio::null())
        .stderr(File::create(&control_log).unwrap())
        .spawn()
        .unwrap();
    let control_deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&control_log)
        .unwrap()
        .contains("file lock")
    {
        assert!(
            control.try_wait().unwrap().is_none(),
            "baseline Cargo did not wait for the held cache lock"
        );
        assert!(
            Instant::now() < control_deadline,
            "baseline Cargo did not report lock contention"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut children = Vec::new();
    let release = fixture.path("release");
    let _release_on_failure = ReleaseOnDrop(release.clone());
    for index in 0..2 {
        let workspace = fixture.path(&format!("consumer-{index}"));
        project(&workspace);
        let ready = fixture.path(&format!("ready-{index}"));
        let mut command = fixture.heli(["--dependency-epoch"]);
        command
            .arg(&selected)
            .args(["--cache", "off", "test", "--offline"])
            .current_dir(&workspace)
            .env("CARGO_HOME", &original)
            .env("CARGO_TARGET_DIR", workspace.join("target"))
            .env("EPOCH_READY", &ready)
            .env("EPOCH_RELEASE", &release)
            .env(
                "EPOCH_SOURCE",
                selected.join("vendor/heli-epoch-dep/src/lib.rs"),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        children.push((command.spawn().unwrap(), ready));
    }
    // Both build scripts must be live before either may finish. A shared
    // home lock or a global epoch lock would make this impossible.
    for (child, ready) in &mut children {
        wait_file(ready, child);
    }
    let homes: Vec<_> = children
        .iter()
        .map(|(_, ready)| std::fs::read_to_string(ready).unwrap())
        .collect();
    assert_ne!(homes[0], homes[1]);
    assert!(homes.iter().all(|home| Path::new(home) != original));
    let report = gc(&fixture, &store);
    assert!(selected.is_dir(), "active epoch lost: {report}");
    assert!(!retired.exists(), "unrelated epoch was blocked: {report}");
    write(&release, "go");
    for (child, _) in children {
        assert_success(
            &child.wait_with_output().unwrap(),
            "concurrent isolated Cargo test",
        );
    }
    // Discovery's captured stdout must remain raw Cargo JSON. A guardian
    // handshake prefix would silently disable target registration.
    // Disk status is the accountant's scanned view, which updates after
    // learning signals the run's end; client completion is not a scan barrier.
    let expected: Vec<_> = (0..2)
        .map(|index| {
            std::fs::canonicalize(fixture.path(&format!("consumer-{index}/target"))).unwrap()
        })
        .collect();
    fixture.wait_for_status(
        "both isolated consumers' metadata targets to be accounted",
        |status| {
            let Some(targets) = status["disk"]["targets"].as_array() else {
                return false;
            };
            expected.iter().all(|expected| {
                targets.iter().any(|entry| {
                    entry["path"]
                        .as_str()
                        .and_then(|path| std::fs::canonicalize(path).ok())
                        .as_ref()
                        == Some(expected)
                })
            })
        },
    );
    assert!(
        control.try_wait().unwrap().is_none(),
        "control lock was released during the isolated workload"
    );
    control.kill().unwrap();
    control.wait().unwrap();
    drop(baseline_lock);
    let report = gc(&fixture, &store);
    assert!(!selected.exists(), "released epoch not reclaimed: {report}");
}

#[test]
fn import_rejects_changed_checksum_without_claiming_unknown_store() {
    let fixture = Fixture::new("basic");
    let source = vendor(&fixture, 7);
    write(&source.0.join("heli-epoch-dep/src/lib.rs"), "tampered");
    let store = fixture.path("unknown-store");
    write(&store.join("keep.txt"), "sole copy");
    let output = fixture
        .heli(["deps", "import", "--store"])
        .arg(&store)
        .arg("--vendor")
        .arg(&source.0)
        .arg("--config")
        .arg(&source.1)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(store.join("keep.txt")).unwrap(),
        "sole copy"
    );
    assert!(!store.join("epochs").exists());
    let store = fixture.path("fresh-store");
    let output = fixture
        .heli(["deps", "import", "--store"])
        .arg(&store)
        .arg("--vendor")
        .arg(&source.0)
        .arg("--config")
        .arg(&source.1)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"),
        "{output:?}"
    );
    assert_eq!(std::fs::read_dir(store.join("epochs")).unwrap().count(), 0);
}

#[test]
fn original_home_origins_credentials_and_workspace_precedence_survive() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("dependency-store");
    let selected = import(&fixture, &store, &vendor(&fixture, 7));
    let original = fixture.path("original/.cargo");
    let credentials = "[registry]\ntoken='private-test-token'\n";
    write(&original.join("credentials.toml"), credentials);
    write(
        &original.join("config.toml"),
        "[env]\nHELI_HOME_CANARY={value='marker',relative=true}\n",
    );
    let release = fixture.path("release");
    write(&release, "go");
    for (index, override_workspace) in [false, true].into_iter().enumerate() {
        let workspace = fixture.path(&format!("consumer-{index}"));
        project(&workspace);
        if override_workspace {
            write(
                &workspace.join(".cargo/config.toml"),
                "[env]\nHELI_HOME_CANARY={value='workspace-wins',relative=false}\n",
            );
        }
        let ready = fixture.path(&format!("ready-{index}"));
        let report = fixture.path(&format!("env-{index}"));
        let output = fixture
            .heli(["--dependency-epoch"])
            .arg(&selected)
            .args(["--cache", "off", "check", "--offline"])
            .current_dir(&workspace)
            .env("CARGO_HOME", &original)
            .env("CARGO_TARGET_DIR", workspace.join("target"))
            .env("EPOCH_READY", &ready)
            .env("EPOCH_RELEASE", &release)
            .env("EPOCH_ENV_REPORT", &report)
            .env(
                "EPOCH_SOURCE",
                selected.join("vendor/heli-epoch-dep/src/lib.rs"),
            )
            .output()
            .unwrap();
        assert_success(&output, "original Cargo configuration through private home");
        let home = PathBuf::from(std::fs::read_to_string(ready).unwrap());
        assert_eq!(
            std::fs::read_to_string(home.join("credentials.toml")).unwrap(),
            credentials
        );
        assert_eq!(
            std::fs::read_to_string(original.join("credentials.toml")).unwrap(),
            credentials
        );
        let canary = std::fs::read_to_string(report).unwrap();
        if override_workspace {
            assert_eq!(canary, "workspace-wins");
        } else {
            assert_eq!(Path::new(&canary), fixture.path("original/marker"));
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-test-token"));
    }
}

#[test]
fn changing_a_published_object_is_refused_before_any_command_runs() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let selected = import(&fixture, &fixture.path("store"), &vendor(&fixture, 7));
    let source = selected.join("vendor/heli-epoch-dep/src/lib.rs");
    let mut permissions = std::fs::metadata(&source).unwrap().permissions();
    #[cfg(windows)]
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "Windows-only deliberate corruption of this test's immutable object"
    )]
    permissions.set_readonly(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o200);
    }
    std::fs::set_permissions(&source, permissions).unwrap();
    std::fs::write(&source, "tampered after publication").unwrap();
    let output = fixture
        .heli(["--dependency-epoch"])
        .arg(&selected)
        .args(["exec", "--"])
        .arg(&echo)
        .arg("must-not-run")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "tampered inputs reached command: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("checksum"),
        "{output:?}"
    );
}

fn ownership_survives_parent_loss(fallback: bool) {
    use support::fake_daemon::{Answer, FakeDaemon};
    let fixture = Fixture::new("basic");
    let store = fixture.path("store");
    let selected = import(&fixture, &store, &vendor(&fixture, 7));
    let fake = fallback.then(|| FakeDaemon::start(&fixture.endpoint(), Answer::Version(1)));
    let workspace = fixture.path("consumer");
    project(&workspace);
    let ready = fixture.path("ready");
    let release = fixture.path("release");
    let done = fixture.path("done");
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let mut child = fixture
        .heli(["--dependency-epoch"])
        .arg(&selected)
        .args(["--cache", "off", "check", "--offline"])
        .current_dir(&workspace)
        .env("CARGO_HOME", fixture.path("original/.cargo"))
        .env("CARGO_TARGET_DIR", workspace.join("target"))
        .env("EPOCH_READY", &ready)
        .env("EPOCH_RELEASE", &release)
        .env("EPOCH_DONE", &done)
        .env(
            "EPOCH_SOURCE",
            selected.join("vendor/heli-epoch-dep/src/lib.rs"),
        )
        .stdout(File::create(fixture.path("stdout")).unwrap())
        .stderr(File::create(fixture.path("stderr")).unwrap())
        .spawn()
        .unwrap();
    wait_file(&ready, &mut child);
    if let Some(fake) = fake {
        fake.received();
    }
    child.kill().unwrap();
    child.wait().unwrap();
    if !fallback {
        support::kill_process(fixture.daemon_pid().expect("the managed daemon"));
    }
    let report = gc(&fixture, &store);
    assert!(
        selected.exists(),
        "parent loss released the running command's epoch: {report}"
    );
    assert!(!done.exists(), "command must still be waiting");
    write(&release, "go");
    let until = Instant::now() + Duration::from_secs(40);
    while !done.exists() {
        assert!(
            Instant::now() < until,
            "orphaned command could not read its dependencies"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    while selected.exists() {
        let report = gc(&fixture, &store);
        assert!(
            Instant::now() < until,
            "guardian retained a completed epoch: {report}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn source_ownership_survives_client_and_daemon_loss() {
    ownership_survives_parent_loss(false);
}

#[test]
fn fallback_guardian_survives_client_loss() {
    ownership_survives_parent_loss(true);
}

#[test]
fn selecting_a_retiring_epoch_fails_promptly_instead_of_waiting_for_gc() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("store");
    let selected = import(&fixture, &store, &vendor(&fixture, 7));
    let lease = store
        .join("leases")
        .join(selected.file_name().unwrap())
        .join(".heli-runtime.lock");
    let held = File::options().read(true).write(true).open(lease).unwrap();
    held.lock().unwrap();
    let mut child = fixture
        .heli(["--dependency-epoch"])
        .arg(&selected)
        .args(["exec", "--", "missing-epoch-test-command"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while Instant::now() < until {
        if child.try_wait().unwrap().is_some() {
            completed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if !completed {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        completed,
        "a retiring epoch must not make the client queue behind GC: {output:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "a command must not start: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("retired"),
        "{output:?}"
    );
}

/// Cargo must notice a different immutable source path even when package
/// version, archive checksum, Cargo.lock and the target directory stay fixed.
#[test]
fn switching_epochs_rebuilds_the_same_locked_consumer_in_the_same_target() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("dependency-store");
    let first_source = vendor(&fixture, 7);
    let second_source = vendor(&fixture, 8);
    let checksum = |source: &(PathBuf, PathBuf)| -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(source.0.join("heli-epoch-dep/.cargo-checksum.json")).unwrap(),
        )
        .unwrap()
    };
    let first_checksum = checksum(&first_source);
    let second_checksum = checksum(&second_source);
    assert_eq!(first_checksum["package"], second_checksum["package"]);
    assert_eq!(
        first_checksum["files"]["Cargo.toml"],
        second_checksum["files"]["Cargo.toml"]
    );
    assert_ne!(
        first_checksum["files"]["src/lib.rs"],
        second_checksum["files"]["src/lib.rs"]
    );
    let first = import(&fixture, &store, &first_source);
    let second = import(&fixture, &store, &second_source);
    assert_ne!(first, second);
    // Keep both source views available throughout the switch. Rebuilding
    // must follow source selection, not disappearance of a previous input.
    let _leases: Vec<File> = [&first, &second]
        .into_iter()
        .map(|epoch| {
            let lease = File::options()
                .read(true)
                .write(true)
                .open(
                    store
                        .join("leases")
                        .join(epoch.file_name().unwrap())
                        .join(".heli-runtime.lock"),
                )
                .unwrap();
            lease.lock_shared().unwrap();
            lease
        })
        .collect();
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let workspace = fixture.path("consumer");
    project(&workspace);
    // This consumer observes the linked value at runtime without the
    // separate lifetime-canary build script used by the other tests.
    std::fs::remove_file(workspace.join("build.rs")).unwrap();
    write(
        &workspace.join("src/main.rs"),
        "fn main() { println!(\"epoch-value={}\", epoch_consumer::value()); }\n",
    );
    let target = workspace.join("target");
    let selected = |epoch: &Path| {
        let mut command = fixture.heli(["--dependency-epoch"]);
        command
            .arg(epoch)
            .args(["--output", "full", "--cache", "off"])
            .current_dir(&workspace)
            .env("CARGO_HOME", &original)
            .env("CARGO_TARGET_DIR", &target);
        command
    };
    let output = selected(&first)
        .args(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert_success(&output, "generate the shared lockfile once");
    let lock = std::fs::read(workspace.join("Cargo.lock")).unwrap();
    for (epoch, value) in [(&first, 7), (&second, 8), (&first, 7)] {
        let output = selected(epoch)
            .args(["run", "--offline", "--locked", "--quiet"])
            .output()
            .unwrap();
        assert_success(
            &output,
            "run the same locked consumer after selecting an epoch",
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("epoch-value={value}"),
            "selected epoch {}: {output:?}",
            epoch.display()
        );
        assert!(target.is_dir());
        assert_eq!(std::fs::read(workspace.join("Cargo.lock")).unwrap(), lock);
        let report = gc(&fixture, &store);
        assert!(
            first.is_dir() && second.is_dir(),
            "source views disappeared: {report}"
        );
        assert_eq!(
            report["busy_epochs"].as_array().unwrap().len(),
            2,
            "{report}"
        );
        assert!(
            report["retired_epochs"].as_array().unwrap().is_empty(),
            "{report}"
        );
    }
}

#[test]
fn orphaned_metadata_keeps_sources_after_the_run_and_daemon_have_ended() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("metadata-sources");
    let selected = import(&fixture, &store, &vendor(&fixture, 7));
    let echo = fixture.echo_args();
    let cargo = fixture.path(&format!("tools/cargo{}", std::env::consts::EXE_SUFFIX));
    std::fs::create_dir_all(cargo.parent().unwrap()).unwrap();
    std::fs::copy(echo, &cargo).unwrap();
    let versions = fixture.path("versions");
    write(&versions.join("cargo"), "cargo 1.99.0 (metadata-fixture)\n");
    let workspace = fixture.path("metadata-consumer");
    project(&workspace);
    let ready = fixture.path("metadata-pid");
    let release = fixture.path("metadata-release");
    let read = fixture.path("metadata-read");
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let mut child = fixture
        .heli(["--dependency-epoch"])
        .arg(&selected)
        .args(["--output", "full", "--cache", "off", "check", "--offline"])
        .current_dir(&workspace)
        .env("CARGO", &cargo)
        .env("CARGO_HOME", fixture.path("original/.cargo"))
        .env("ECHO_VERSION", &versions)
        .env("ECHO_METADATA_PID", &ready)
        .env("ECHO_METADATA_WAIT_FOR", &release)
        .env(
            "ECHO_METADATA_SOURCE",
            selected.join("vendor/heli-epoch-dep/src/lib.rs"),
        )
        .env("ECHO_METADATA_READ", &read)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_file(&ready, &mut child);
    let metadata: u32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    // The real command and its guardian must end first. The client's
    // bounded learning wait expires while the separate metadata stays gated.
    let until = Instant::now() + Duration::from_secs(35);
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < until,
            "completed command still waits for metadata"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert_success(&output, "the foreground command ends before discovery");
    assert!(support::process_runs(metadata));
    assert!(!read.exists());
    let report = gc(&fixture, &store);
    assert!(
        selected.exists(),
        "live daemon must retain discovery inputs: {report}"
    );
    let daemon = fixture.daemon_pid().expect("this fixture's daemon");
    support::kill_process(daemon);
    while support::process_runs(daemon) {
        assert!(Instant::now() < until, "fixture daemon did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        support::process_runs(metadata),
        "the metadata consumer must survive"
    );
    let report = gc(&fixture, &store);
    assert!(
        selected.exists(),
        "GC retired a live orphan's inputs: {report}"
    );
    assert_eq!(
        report["busy_epochs"].as_array().unwrap().len(),
        1,
        "{report}"
    );
    write(&release, "go");
    while !read.exists() {
        assert!(
            Instant::now() < until,
            "orphan metadata never reported its late read"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(&read).unwrap(),
        "READ_OK:pub const VALUE: u32 = 7;\n"
    );
    while selected.exists() {
        let report = gc(&fixture, &store);
        assert!(
            Instant::now() < until,
            "completed metadata retained its epoch: {report}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A post-checkout hook inherits the selected context but bypasses client::run.
/// Its discovery query must retain sources even after that hook process dies.
#[test]
fn inherited_epoch_protects_recycle_metadata_after_its_parent_is_killed() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("metadata-sources");
    let selected = import(&fixture, &store, &vendor(&fixture, 7));
    let echo = fixture.echo_args();
    let cargo = fixture.path(&format!("tools/cargo{}", std::env::consts::EXE_SUFFIX));
    std::fs::create_dir_all(cargo.parent().unwrap()).unwrap();
    std::fs::copy(echo, &cargo).unwrap();
    let versions = fixture.path("versions");
    write(&versions.join("cargo"), "cargo 1.99.0 (metadata-fixture)\n");
    let workspace = fixture.path("hook-consumer");
    project(&workspace);
    // Like the recycling fixtures, provide an orphan of this repository.
    // Without one the hook correctly avoids starting Cargo metadata at all.
    let artifacts = fixture.path("artifacts");
    fixture.configure(&format!("[store]\npaths=[{}]\n", quoted(&artifacts)));
    // Git resolves aliases such as macOS /var -> /private/var. Owners
    // produced by discovery carry Git's repository identity, not the
    // spelling used to create this temporary workspace.
    let repository = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert_success(&repository, "identify the fixture repository");
    let repository = PathBuf::from(String::from_utf8(repository.stdout).unwrap().trim());
    let owner = serde_json::json!({
        "workspace": fixture.path("removed-worktree"),
        "repository": repository,
        "created_at": 1,
        "last_run": 1,
    });
    write(&artifacts.join("aa/bbbb/.heli/owner"), owner.to_string());
    // Reproduce the environment inherited by a hook in an isolated run.
    // There is deliberately no --dependency-epoch client preparation here.
    let home = fixture.path("inherited-home");
    write(
        &home.join("config.toml"),
        format!("include=[{}]\n", quoted(&selected.join("config.toml"))),
    );
    let ready = fixture.path("metadata-pid");
    let release = fixture.path("metadata-release");
    let read = fixture.path("metadata-read");
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let mut child = fixture
        .heli(["recycle"])
        .current_dir(&workspace)
        .env("CARGO", &cargo)
        .env("CARGO_HOME", &home)
        .env("HELI_DEPENDENCY_EPOCH", &selected)
        .env("ECHO_VERSION", &versions)
        .env("ECHO_METADATA_PID", &ready)
        .env("ECHO_METADATA_WAIT_FOR", &release)
        .env(
            "ECHO_METADATA_SOURCE",
            selected.join("vendor/heli-epoch-dep/src/lib.rs"),
        )
        .env("ECHO_METADATA_READ", &read)
        .stdout(Stdio::null())
        .stderr(File::create(fixture.path("stderr")).unwrap())
        .spawn()
        .unwrap();
    wait_file(&ready, &mut child);
    let metadata: u32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    // Kill only the direct hook parent, leaving the independent query alive.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(support::process_runs(metadata));
    let report = gc(&fixture, &store);
    assert!(
        selected.is_dir(),
        "orphan hook metadata lost its sources: {report}"
    );
    assert_eq!(
        report["busy_epochs"].as_array().unwrap().len(),
        1,
        "{report}"
    );
    assert!(support::process_runs(metadata));
    write(&release, "go");
    let until = Instant::now() + Duration::from_secs(40);
    while !read.is_file() {
        assert!(
            Instant::now() < until,
            "hook metadata never completed its late read"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(&read).unwrap(),
        "READ_OK:pub const VALUE: u32 = 7;\n"
    );
    while selected.exists() {
        let report = gc(&fixture, &store);
        assert!(
            Instant::now() < until,
            "completed hook metadata retained its sources: {report}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
