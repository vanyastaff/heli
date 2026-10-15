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
