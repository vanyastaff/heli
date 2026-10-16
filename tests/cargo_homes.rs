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
    let report = env::var_os("HOME_REPORT").unwrap();
    fs::write(report, format!("{}\n{}", env::var("CARGO_HOME").unwrap(),
        env::var("HELI_HOME_CANARY").unwrap_or_default())).unwrap();
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
        vec!["install", "--list"],
        vec!["uninstall", "not-installed"],
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
    // --list is read only: even the pre-fix implementation cannot install
    // packages, change credentials, or reach a registry in these cases.
    for options in [vec!["--color", "never"], vec!["--color=never"]] {
        let mut command = fixture.heli(["--cargo-home", "isolated", "exec", "--"]);
        command
            .arg(support::cargo_binary())
            .args(options)
            .args(["install", "--list"])
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
