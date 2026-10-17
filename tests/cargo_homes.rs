//! Native Cargo-home isolation through the public CLI, without vendoring.
//! Registry cold/warm offline acceptance lives in the local-registry harness.
mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::{Fixture, assert_success};

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let hooks = root.join(".cargo-home-test-no-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let output = Command::new("git")
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()))
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert_success(&output, "prepare test-owned Git repository");
}

/// A real offline package whose build script reports its effective Cargo home.
fn project(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\n[package]\nname='cargo-home-canary'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(&root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n");
    write(
        &root.join("build.rs"),
        r#"
use std::{env, fs, path::Path, process::Command, thread, time::{Duration, Instant}};
fn main() {
    println!("cargo:rerun-if-env-changed=HOME_REPORT");
    // Whole or not at all: the test reads the report once it exists.
    let report = Path::new(&env::var_os("HOME_REPORT").unwrap()).to_owned();
    let partial = report.with_extension("partial");
    fs::write(&partial, format!("{}\n{}", env::var("CARGO_HOME").unwrap(),
        env::var("HELI_HOME_CANARY").unwrap_or_default())).unwrap();
    fs::rename(&partial, &report).unwrap();
    if let Some(release) = env::var_os("HOME_RELEASE") {
        let start = Instant::now();
        while !Path::new(&release).is_file() {
            assert!(start.elapsed() < Duration::from_secs(45), "bounded home gate");
            thread::sleep(Duration::from_millis(20));
        }
    }
    if let Some(project) = env::var_os("NESTED_PROJECT") {
        fs::write(env::var_os("ROTATE_CONFIG").unwrap(), "[net]\nretry=2\n").unwrap();
        let mut nested = Command::new(env::var_os("HELI_UNDER_TEST").unwrap());
        nested.args(["--cache", "off", "--output", "full"]);
        let reject_shared = env::var_os("NESTED_SHARED").is_some();
        if reject_shared { nested.args(["--cargo-home", "shared"]); }
        let result = nested.args(["check", "--offline"])
            .current_dir(project)
            .env("HOME_REPORT", env::var_os("NESTED_REPORT").unwrap())
            .env("CARGO_TARGET_DIR", env::var_os("NESTED_TARGET").unwrap())
            .env_remove("NESTED_PROJECT").env_remove("NESTED_SHARED")
            .env_remove("HOME_RELEASE").env_remove("ROTATE_CONFIG")
            .output().unwrap();
        if reject_shared {
            assert_eq!(result.status.code(), Some(2), "nested shared must refuse: {result:?}");
            assert!(String::from_utf8_lossy(&result.stderr).contains("shared"), "{result:?}");
            fs::write(env::var_os("NESTED_REPORT").unwrap(), "shared refused").unwrap();
        } else {
            assert!(result.status.success(), "nested heli failed: {result:?}");
        }
    }
}
"#,
    );
    git(root, &["init", "--quiet"]);
}

fn selected(fixture: &Fixture, root: &Path, original: &Path, report: &Path) -> Command {
    let mut command = fixture.heli(["--cache", "off", "--output", "full"]);
    command
        .current_dir(root)
        .env("CARGO_HOME", original)
        .env("CARGO_TARGET_DIR", report.with_extension("target"))
        .env("HOME_REPORT", report);
    command
}

fn reported(path: &Path) -> (PathBuf, String) {
    let text = std::fs::read_to_string(path).unwrap();
    let (home, canary) = text.split_once('\n').unwrap();
    (PathBuf::from(home), canary.to_owned())
}

fn wait_report(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !path.is_file() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "client exited before {}",
            path.display()
        );
        assert!(
            Instant::now() < deadline,
            "no build-script report: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn bounded_output(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("public Cargo-home command timed out: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

struct ReleaseOnDrop(PathBuf);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "go");
    }
}

#[test]
fn two_real_git_worktrees_get_distinct_stable_native_homes() {
    let fixture = Fixture::new("basic");
    let first = fixture.path("first");
    project(&first);
    git(&first, &["add", "."]);
    git(
        &first,
        &[
            "-c",
            "user.name=heli",
            "-c",
            "user.email=heli@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "canary",
        ],
    );
    let second = fixture.path("second");
    git(
        &first,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            second.to_str().unwrap(),
            "HEAD",
        ],
    );
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let release = fixture.path("release");
    let _release = ReleaseOnDrop(release.clone());
    let mut children = Vec::new();
    for (index, root) in [&first, &second].into_iter().enumerate() {
        let report = fixture.path(&format!("report-{index}"));
        let child = selected(&fixture, root, &original, &report)
            .args(["--cargo-home", "isolated", "check", "--offline"])
            .env("HOME_RELEASE", &release)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(fixture.path(&format!("stderr-{index}"))).unwrap())
            .spawn()
            .unwrap();
        children.push((child, report));
    }
    for (child, report) in &mut children {
        wait_report(report, child);
    }
    let homes: Vec<PathBuf> = children
        .iter()
        .map(|(_, report)| reported(report).0)
        .collect();
    assert_ne!(
        homes[0], homes[1],
        "different Git worktrees must not share mutable native Cargo caches"
    );
    assert!(homes.iter().all(|home| home != &original));
    assert!(
        !original.join("registry").exists(),
        "native worktree homes must not use the original registry cache"
    );
    write(&release, "go");
    for (child, _) in &mut children {
        assert!(child.wait().unwrap().success());
    }
    for (index, root) in [&first, &second].into_iter().enumerate() {
        let report = fixture.path(&format!("repeat-{index}"));
        let output = bounded_output(selected(&fixture, root, &original, &report).args([
            "--cargo-home",
            "isolated",
            "check",
            "--offline",
        ]));
        assert_success(
            &output,
            "warm native isolated home without source preparation",
        );
        assert_eq!(reported(&report).0, homes[index]);
    }
}

#[test]
fn shared_is_the_default_and_config_opt_in_can_be_explicitly_overridden() {
    let fixture = Fixture::new("basic");
    let root = fixture.path("consumer");
    project(&root);
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let report = fixture.path("default-report");
    let output =
        bounded_output(selected(&fixture, &root, &original, &report).args(["check", "--offline"]));
    assert_success(&output, "default native shared home");
    assert_eq!(reported(&report).0, original);
    fixture.configure("[cargo]\nhome='isolated'\n");
    let report = fixture.path("configured-report");
    let output =
        bounded_output(selected(&fixture, &root, &original, &report).args(["check", "--offline"]));
    assert_success(&output, "configuration opts into isolated native home");
    assert_ne!(reported(&report).0, original);
    let report = fixture.path("explicit-shared-report");
    let output = bounded_output(selected(&fixture, &root, &original, &report).args([
        "--cargo-home",
        "shared",
        "check",
        "--offline",
    ]));
    assert_success(
        &output,
        "explicit shared overrides configured isolated at top level",
    );
    assert_eq!(reported(&report).0, original);
}

#[test]
fn original_config_origins_credentials_and_workspace_precedence_survive() {
    let fixture = Fixture::new("basic");
    let original = fixture.path("original/.cargo");
    let secret = "[registry]\ntoken='private-native-home-canary'\n";
    write(&original.join("credentials.toml"), secret);
    write(
        &original.join("config.toml"),
        "[env]\nHELI_HOME_CANARY={value='marker',relative=true}\n",
    );
    for override_workspace in [false, true] {
        let root = fixture.path(if override_workspace {
            "overridden"
        } else {
            "included"
        });
        project(&root);
        if override_workspace {
            write(
                &root.join(".cargo/config.toml"),
                "[env]\nHELI_HOME_CANARY={value='workspace-wins',relative=false}\n",
            );
        }
        let report = root.join("observed-home");
        let output = bounded_output(selected(&fixture, &root, &original, &report).args([
            "--cargo-home",
            "isolated",
            "check",
            "--offline",
        ]));
        assert_success(
            &output,
            "native isolated home preserves Cargo precedence and path origins",
        );
        let (home, value) = reported(&report);
        assert_ne!(home, original);
        if override_workspace {
            assert_eq!(value, "workspace-wins");
        } else {
            assert_eq!(PathBuf::from(value), fixture.path("original/marker"));
        }
        assert_eq!(
            std::fs::read_to_string(home.join("credentials.toml")).unwrap(),
            secret
        );
        assert_eq!(
            std::fs::read_to_string(original.join("credentials.toml")).unwrap(),
            secret
        );
        assert_private(&home);
        assert_private(&home.join("credentials.toml"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private-native-home-canary"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-native-home-canary"));
    }
}

#[test]
fn nested_generated_project_reuses_exact_home_after_original_config_rotation() {
    nested(false);
}

#[test]
fn nested_explicit_shared_refuses_instead_of_escaping_the_inherited_home() {
    nested(true);
}

fn nested(shared: bool) {
    let fixture = Fixture::new("basic");
    let outer = fixture.path("outer");
    let generated = fixture.path("generated/inner");
    project(&outer);
    project(&generated);
    let original = fixture.path("original/.cargo");
    write(&original.join("config.toml"), "[net]\nretry=1\n");
    let report = fixture.path("outer-home");
    let nested_report = fixture.path("inner-home");
    let mut command = selected(&fixture, &outer, &original, &report);
    command
        .args(["--cargo-home", "isolated", "check", "--offline"])
        .env("HELI_UNDER_TEST", support::heli_binary())
        .env("NESTED_PROJECT", &generated)
        .env("ROTATE_CONFIG", original.join("config.toml"))
        .env("NESTED_REPORT", &nested_report)
        .env("NESTED_TARGET", fixture.path("inner-target"));
    if shared {
        command.env("NESTED_SHARED", "1");
    }
    let output = bounded_output(&mut command);
    assert_success(&output, "outer Cargo observes the nested home contract");
    let (home, _) = reported(&report);
    assert_ne!(home, original);
    assert_eq!(
        std::fs::read_to_string(original.join("config.toml")).unwrap(),
        "[net]\nretry=2\n"
    );
    if shared {
        assert_eq!(
            std::fs::read_to_string(nested_report).unwrap(),
            "shared refused"
        );
    } else {
        assert_eq!(
            reported(&nested_report).0,
            home,
            "generated cwd and rotated config must not create an include/home chain"
        );
    }
}

#[test]
fn native_home_mutating_commands_are_refused_before_execution() {
    let fixture = Fixture::new("basic");
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    for args in [
        vec!["install", "not-a-crate"],
        vec!["uninstall", "not-installed"],
        vec!["binstall", "--dry-run", "not-a-crate"],
        vec!["install-update", "-a"],
        vec!["quickinstall", "not-a-crate"],
        vec!["login", "not-a-real-token"],
        vec!["logout"],
        vec!["publish", "--dry-run", "--offline"],
    ] {
        let output = fixture
            .heli(["--cargo-home", "isolated"])
            .args(&args)
            .env("CARGO_HOME", &original)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "isolated {}: {output:?}",
            args[0]
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("isolated") && error.contains(args[0]),
            "{error}"
        );
        assert!(!original.join("credentials.toml").exists());
        assert!(!original.join(".crates.toml").exists());
    }
}

/// `install --list` only reads the record of installed tools, so it is not
/// refused; it lists the private home's, which is empty.
#[test]
fn install_list_is_not_refused_under_an_isolated_home() {
    let fixture = Fixture::new("basic");
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let output = fixture
        .heli(["--cargo-home", "isolated"])
        .args(["install", "--list"])
        .env("CARGO_HOME", &original)
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!error.contains("not supported"), "{error}");
    assert_success(&output, "install --list under an isolated home");
    assert!(!original.join(".crates.toml").exists());
}

#[test]
fn configured_isolation_cannot_be_lost_when_state_locations_are_unavailable() {
    let fixture = Fixture::new("basic");
    fixture.configure("[cargo]\nhome='isolated'\n");
    let root = fixture.path("consumer");
    project(&root);
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let report = fixture.path("must-not-run");
    // Resolve actual binaries before removing HOME on Unix; rustup's shim
    // must not turn a wrong shared fallback into an unrelated toolchain error.
    let native = |tool: &str| {
        let output = Command::new("rustup")
            .args(["which", tool])
            .output()
            .unwrap();
        assert_success(&output, "locate the selected native toolchain");
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    };
    let mut command = selected(&fixture, &root, &original, &report);
    command
        .args(["check", "--offline"])
        .env("CARGO", native("cargo"))
        .env("RUSTC", native("rustc"))
        .env_remove("HELI_STATE_DIR");
    #[cfg(windows)]
    command.env_remove("LOCALAPPDATA");
    #[cfg(unix)]
    command.env_remove("XDG_STATE_HOME").env_remove("HOME");
    let output = bounded_output(&mut command);
    assert_eq!(
        output.status.code(),
        Some(2),
        "state discovery must not downgrade configured isolation: {output:?}"
    );
    assert!(
        !report.exists(),
        "Cargo executed after dropping the configured isolated-home policy"
    );
    assert!(
        !original.join(".package-cache").exists(),
        "the original native cache was opened"
    );
}

#[test]
fn cargo_global_options_do_not_bypass_isolated_home_command_refusal() {
    let fixture = Fixture::new("basic");
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    // The cases are refused before Cargo runs, so nothing can be installed.
    for options in [vec!["--color", "never"], vec!["--color=never"]] {
        let mut command = fixture.heli(["--cargo-home", "isolated", "exec", "--"]);
        command
            .arg(support::cargo_binary())
            .args(options)
            .args(["install", "not-a-crate"])
            .env("CARGO_HOME", &original);
        let output = bounded_output(&mut command);
        assert_eq!(
            output.status.code(),
            Some(2),
            "leading Cargo options escaped the command policy: {output:?}"
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("isolated") && error.contains("install"),
            "{error}"
        );
        assert!(!original.join(".crates.toml").exists());
    }
}

#[test]
fn native_isolated_fallback_keeps_runtime_ownership_after_client_loss() {
    use std::fs::{File, TryLockError};
    use support::fake_daemon::{Answer, FakeDaemon};

    let fixture = Fixture::new("basic");
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Version(1));
    let root = fixture.path("fallback-consumer");
    project(&root);
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let report = fixture.path("fallback-home");
    let target = report.with_extension("target");
    let release = fixture.path("fallback-release");
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let activity = fixture.path("native-fallback-activity");
    let stderr = fixture.path("fallback-stderr");
    let mut client = selected(&fixture, &root, &original, &report)
        .args(["--cargo-home", "isolated", "check", "--offline"])
        .env("HELI_ACTIVITY_DIR", &activity)
        .env("HOME_RELEASE", &release)
        .stdout(File::create(fixture.path("fallback-stdout")).unwrap())
        .stderr(File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    wait_report(&report, &mut client);
    fake.received();
    let (home, _) = reported(&report);
    assert_ne!(home, original);
    assert!(
        std::fs::read_to_string(&stderr)
            .unwrap()
            .contains("running the command directly"),
        "the acceptance must exercise direct fallback"
    );

    client.kill().unwrap();
    client.wait().unwrap();
    // This is the same external exclusive-lock attempt GC makes. The
    // command is still gated after the submitting CLI has disappeared.
    let external_gc = File::options()
        .read(true)
        .write(true)
        .open(activity.join(".heli-runtime.lock"))
        .unwrap();
    assert!(
        matches!(external_gc.try_lock(), Err(TryLockError::WouldBlock)),
        "client loss must not expose the live fallback command to GC"
    );
    assert!(home.join("config.toml").is_file());
    assert_private(&home);
    assert!(!original.join(".package-cache").exists());
    write(&release, "go");

    let deadline = Instant::now() + Duration::from_secs(40);
    let has_metadata = || {
        std::fs::read_dir(target.join("debug/deps"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("libcargo_home_canary-") && name.ends_with(".rmeta")
            })
    };
    while !has_metadata() {
        assert!(
            Instant::now() < deadline,
            "orphaned native Cargo did not finish: {}",
            std::fs::read_to_string(&stderr).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    loop {
        match external_gc.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                assert!(
                    Instant::now() < deadline,
                    "completed fallback retained its runtime guard"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::Error(error)) => panic!("external GC lock failed: {error}"),
        }
    }
    assert!(home.join("config.toml").is_file());
}

#[test]
fn invalid_daemon_memory_settings_keep_native_isolation_during_direct_fallback() {
    let fixture = Fixture::new("basic");
    fixture.configure("[cargo]\nhome='isolated'\n[memory]\nlow_watermark_mb='lots'\n");
    let outer = fixture.path("fallback-outer");
    let generated = fixture.path("fallback-generated/inner");
    project(&outer);
    project(&generated);
    let original = fixture.path("original/.cargo");
    let config = "[env]\nHELI_HOME_CANARY='original-config'\n";
    let credentials = "[registry]\ntoken='fallback-private-native-canary'\n";
    write(&original.join("config.toml"), config);
    write(&original.join("credentials.toml"), credentials);
    let report = fixture.path("fallback-outer-home");
    let nested_report = fixture.path("fallback-inner-home");
    let output = bounded_output(
        selected(&fixture, &outer, &original, &report)
            .args(["check", "--offline"])
            .env("HELI_UNDER_TEST", support::heli_binary())
            .env("NESTED_PROJECT", &generated)
            // The common helper writes this optional canary before nesting;
            // preserve the actual original configuration in this acceptance.
            .env("ROTATE_CONFIG", fixture.path("unused-rotation-config.toml"))
            .env("NESTED_REPORT", &nested_report)
            .env("NESTED_TARGET", fixture.path("fallback-inner-target")),
    );
    assert_success(
        &output,
        "unusable daemon policy keeps isolated native direct fallback",
    );
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostics.contains("warning") && diagnostics.contains("running the command directly"),
        "{diagnostics}"
    );
    let (home, canary) = reported(&report);
    assert_ne!(home, original);
    assert_eq!(canary, "original-config");
    assert_eq!(
        reported(&nested_report),
        (home.clone(), "original-config".into())
    );
    assert!(home.join("config.toml").is_file());
    assert_eq!(
        std::fs::read_to_string(home.join("credentials.toml")).unwrap(),
        credentials
    );
    assert_eq!(
        std::fs::read_to_string(original.join("credentials.toml")).unwrap(),
        credentials
    );
    assert_eq!(
        std::fs::read_to_string(original.join("config.toml")).unwrap(),
        config
    );
    assert!(!original.join(".package-cache").exists());
    assert_private(&home);
    assert_private(&home.join("credentials.toml"));
    assert!(!diagnostics.contains("fallback-private-native-canary"));
}

#[test]
fn mismatched_native_home_full_identity_refuses_reuse_without_replacing_live_data() {
    let fixture = Fixture::new("basic");
    let root = fixture.path("identity-consumer");
    project(&root);
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let first_report = fixture.path("identity-first-home");
    let output = bounded_output(selected(&fixture, &root, &original, &first_report).args([
        "--cargo-home",
        "isolated",
        "check",
        "--offline",
    ]));
    assert_success(&output, "prepare a legitimate native Cargo home");
    let home = reported(&first_report).0;
    let marker_path = home.join(".heli-private-home.json");
    let original_marker = std::fs::read(&marker_path).unwrap();
    let original_config = std::fs::read(home.join("config.toml")).unwrap();
    let persistent = home.join("acceptance-persistent-cache");
    write(
        &persistent,
        "private cached bytes must survive a collision refusal",
    );
    let mut marker: serde_json::Value = serde_json::from_slice(&original_marker).unwrap();
    let mut full_identity = marker["home_identity"]
        .as_str()
        .expect("native private home records its complete identity")
        .as_bytes()
        .to_vec();
    // Change only the trailing digest byte. The directory's compact prefix
    // still matches; accepting that prefix alone must not authorize reuse.
    let last = full_identity.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    marker["home_identity"] = String::from_utf8(full_identity).unwrap().into();
    let tampered_marker = serde_json::to_vec(&marker).unwrap();
    std::fs::write(&marker_path, &tampered_marker).unwrap();

    let refused_report = fixture.path("identity-must-not-run");
    let output = bounded_output(selected(&fixture, &root, &original, &refused_report).args([
        "--cargo-home",
        "isolated",
        "check",
        "--offline",
    ]));
    assert_eq!(
        output.status.code(),
        Some(2),
        "mismatched full identity was reused: {output:?}"
    );
    assert!(
        !refused_report.exists(),
        "Cargo's build script ran after identity validation failed"
    );
    assert!(
        !refused_report.with_extension("target").exists(),
        "a native build was started after identity refusal"
    );
    assert_eq!(std::fs::read(&marker_path).unwrap(), tampered_marker);
    assert_eq!(
        std::fs::read(home.join("config.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        std::fs::read_to_string(&persistent).unwrap(),
        "private cached bytes must survive a collision refusal"
    );

    // The caller repairs its own deliberate tamper. Heli must retain the
    // legitimate existing cache instead of silently replacing the directory.
    std::fs::write(&marker_path, original_marker).unwrap();
    let restored_report = fixture.path("identity-restored-home");
    let output = bounded_output(
        selected(&fixture, &root, &original, &restored_report).args([
            "--cargo-home",
            "isolated",
            "check",
            "--offline",
        ]),
    );
    assert_success(
        &output,
        "reuse the legitimate home after explicit marker restoration",
    );
    assert_eq!(reported(&restored_report).0, home);
    assert_eq!(
        std::fs::read_to_string(persistent).unwrap(),
        "private cached bytes must survive a collision refusal"
    );
}

/// Real aws-lc sources reach this depth in Cargo's native registry cache.
/// Rust can create/read a long path while MSVC still refuses to open it.
#[cfg(windows)]
#[test]
fn isolated_native_home_can_compile_real_msvc_source_at_aws_lc_depth() {
    let fixture = Fixture::new("basic");
    let root = fixture.path("msvc-consumer");
    project(&root);
    write(
        &root.join("build.rs"),
        r#"
use std::{env, fs, path::PathBuf, process::Command};
fn main() {
    let home = PathBuf::from(env::var_os("CARGO_HOME").unwrap());
    let source = home.join("registry/src/index.crates.io-1949cf8c6b5b557f/aws-lc-sys-0.45.0/aws-lc/third_party/jitterentropy/jitterentropy-library/src/jitterentropy-base.c");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "int answer(void) { return 42; }\n").unwrap();
    assert!(source.is_file());
    assert_eq!(fs::read_to_string(&source).unwrap(), "int answer(void) { return 42; }\n");
    fs::write(env::var_os("HOME_REPORT").unwrap(), format!("{}\n", home.display())).unwrap();
    println!("cargo:warning=MSVC native source exists at {}", source.display());
    let object = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("probe.obj");
    let output = Command::new(env::var_os("HOME_TEST_CL").unwrap())
        .args(["/nologo", "/c"])
        .arg(format!("/Fo{}", object.display()))
        .arg(&source).env_remove("CL").env_remove("_CL_")
        .output().expect("run the installed native MSVC compiler");
    assert!(output.status.success(), "MSVC could not compile existing native cache source {}:\n{}\n{}",
        source.display(), String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(object.is_file(), "MSVC did not produce its object file");
}
"#,
    );
    let original = fixture.path("original/.cargo");
    std::fs::create_dir_all(&original).unwrap();
    let report = fixture.path("msvc-home");
    let output = bounded_output(
        selected(&fixture, &root, &original, &report)
            .args(["--cargo-home", "isolated", "check", "--offline"])
            .env("HOME_TEST_CL", installed_msvc()),
    );
    assert_success(
        &output,
        "isolated native Cargo-home sources remain consumable by MSVC",
    );
    assert_ne!(reported(&report).0, original);
}

#[cfg(windows)]
#[test]
fn native_msvc_acceptance_works_without_program_files_environment() {
    // Strip both environment-root discovery and PATH discovery in a child,
    // without changing this test process or other concurrently running tests.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::env::split_paths(&path).filter(|directory| {
        !directory.join("vswhere.exe").is_file() && !directory.join("cl.exe").is_file()
    }))
    .expect("PATH without MSVC discovery tools");
    let output = bounded_output(
        Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "isolated_native_home_can_compile_real_msvc_source_at_aws_lc_depth",
                "--nocapture",
            ])
            .env_remove("ProgramFiles")
            .env_remove("ProgramFiles(x86)")
            .env_remove("ProgramW6432")
            .env("PATH", path)
            .stdin(Stdio::null()),
    );
    assert_success(
        &output,
        "real MSVC compilation without ProgramFiles or tool PATH entries",
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored"),
        "the real native acceptance must run, not skip: {output:?}"
    );
}

#[cfg(windows)]
fn program_files_roots() -> Vec<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, SHGetKnownFolderPath,
    };

    let mut roots = Vec::new();
    for folder in [FOLDERID_ProgramFilesX86, FOLDERID_ProgramFiles] {
        let mut path = std::ptr::null_mut();
        // SAFETY: valid known-folder GUID and output pointer; a null token
        // queries the current user. The allocation is freed below on both
        // success and failure, as required by SHGetKnownFolderPath.
        let result = unsafe { SHGetKnownFolderPath(&folder, 0, std::ptr::null_mut(), &mut path) };
        if result >= 0 && !path.is_null() {
            let mut len = 0;
            // SAFETY: a successful query returns a NUL-terminated UTF-16
            // string. Read it only until that terminator, before freeing it.
            unsafe {
                while *path.add(len) != 0 {
                    len += 1;
                }
                roots.push(PathBuf::from(OsString::from_wide(
                    std::slice::from_raw_parts(path, len),
                )));
            }
        } else {
            eprintln!("MSVC discovery: Windows known-folder query failed: {result:#x}");
        }
        // SAFETY: this is the allocation returned by the query, or null
        // (which CoTaskMemFree permits). It is no longer read afterwards.
        unsafe { CoTaskMemFree(path.cast()) };
    }
    roots
}

#[cfg(windows)]
fn installed_msvc() -> PathBuf {
    let mut candidates = Vec::new();
    for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(root) = std::env::var_os(variable) {
            candidates
                .push(PathBuf::from(root).join("Microsoft Visual Studio/Installer/vswhere.exe"));
        }
    }
    candidates.extend(
        program_files_roots()
            .into_iter()
            .map(|root| root.join("Microsoft Visual Studio/Installer/vswhere.exe")),
    );
    candidates.push("vswhere.exe".into());
    for candidate in &candidates {
        let output = match Command::new(candidate)
            .args([
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-find",
                r"VC\Tools\MSVC\**\bin\Hostx64\x64\cl.exe",
            ])
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("cannot locate native MSVC with vswhere: {error}"),
        };
        assert_success(
            &output,
            "locate installed MSVC for the Windows native source-path acceptance",
        );
        if let Some(path) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .find(|path| path.is_file())
        {
            return path;
        }
    }
    if let Some(path) = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join("cl.exe"))
        .find(|path| path.is_file())
    {
        return path;
    }
    panic!(
        "Windows native Cargo-home acceptance requires installed MSVC C++ tools. \
         Searched vswhere at {candidates:?} (environment roots, Windows known folders, PATH), \
         then cl.exe on PATH. Install the Visual Studio C++ tools or expose an existing \
         vswhere.exe/cl.exe on PATH; this required platform check cannot be silently skipped"
    );
}

#[cfg(unix)]
fn assert_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).unwrap();
    assert_eq!(
        metadata.permissions().mode() & 0o077,
        0,
        "{} must be owner-private",
        path.display()
    );
}

#[cfg(windows)]
fn assert_private(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: nul-terminated path; returned pointers belong to descriptor.
    let result = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(result, ERROR_SUCCESS);
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor was successfully returned and remains live below.
    let mut private = unsafe {
        !owner.is_null()
            && !dacl.is_null()
            && GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) != 0
            && (!path.is_dir() || control & SE_DACL_PROTECTED != 0)
    };
    if private {
        // SAFETY: non-null ACL returned by Windows, all ACEs/SIDs remain live.
        unsafe {
            private = (*dacl).AceCount > 0;
            for index in 0..u32::from((*dacl).AceCount) {
                let mut entry = null_mut();
                if GetAce(dacl, index, &mut entry) == 0 {
                    private = false;
                    break;
                }
                if (*entry.cast::<ACE_HEADER>()).AceType != 0 {
                    private = false;
                    break;
                }
                let sid = (&raw const (*entry.cast::<ACCESS_ALLOWED_ACE>()).SidStart)
                    .cast_mut()
                    .cast();
                if EqualSid(owner, sid) == 0 {
                    private = false;
                    break;
                }
            }
        }
    }
    // SAFETY: Windows allocated this descriptor with LocalAlloc.
    unsafe {
        LocalFree(descriptor);
    }
    assert!(private, "{} grants access beyond its owner", path.display());
}
