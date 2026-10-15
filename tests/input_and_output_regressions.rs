//! Compiler input identity and preservation of diagnostic output.
mod support;

use support::{Fixture, assert_success, split_footer};

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert_success(&output, "audit git setup");
}

fn commit(dir: &std::path::Path) {
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.name=audit",
            "-c",
            "user.email=audit@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "-qm",
            "fixture",
        ],
    );
}

#[test]
fn a_manifest_selected_outside_the_callers_tree_is_part_of_the_cache_input() {
    let fixture = Fixture::new("basic");
    let workspace = fixture.workspace();
    let selected = fixture.path("selected-workspace");
    std::fs::create_dir_all(selected.join("src")).unwrap();
    std::fs::write(
        selected.join("Cargo.toml"),
        "[package]\nname = \"audit-selected\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(selected.join("src/lib.rs"), "pub const VALUE: u8 = 1;\n").unwrap();
    let manifest = selected.join("Cargo.toml");
    let lock = fixture
        .cargo(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(&manifest)
        .output()
        .unwrap();
    assert_success(&lock, "generate selected workspace lockfile");
    git(&workspace, &["init", "-q"]);
    commit(&workspace);
    git(&selected, &["init", "-q"]);
    commit(&selected);
    let check = || {
        let mut command = fixture.heli(["check", "--manifest-path"]);
        command.arg(&manifest);
        command
    };
    let first = check().output().unwrap();
    assert_success(&first, "initial check of selected workspace");
    let hit = check().output().unwrap();
    assert_success(&hit, "repeat selected workspace check");
    assert!(
        String::from_utf8_lossy(&hit.stderr).contains("already passed"),
        "{hit:?}"
    );
    std::fs::write(
        selected.join("src/lib.rs"),
        "compile_error!(\"changed selected source\");\n",
    )
    .unwrap();
    let second = check().output().unwrap();
    let forced = fixture
        .heli(["--no-cache", "check", "--manifest-path"])
        .arg(&manifest)
        .output()
        .unwrap();
    assert_eq!(
        forced.status.code(),
        Some(101),
        "changed selected workspace must fail: {forced:?}"
    );
    assert_eq!(
        second.status.code(),
        Some(101),
        "a changed --manifest-path workspace returned a false green: {second:?}"
    );
}

#[test]
fn a_local_registry_patch_is_part_of_the_cache_input() {
    let fixture = Fixture::new("basic");
    let workspace = fixture.workspace();
    let dependency = fixture.path("patched-dependency");
    std::fs::create_dir_all(dependency.join("src")).unwrap();
    std::fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"local-patch\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    let source = dependency.join("src/lib.rs");
    std::fs::write(&source, "pub const VALUE: u8 = 1;\n").unwrap();
    git(&dependency, &["init", "-q"]);
    commit(&dependency);
    let manifest = workspace.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        format!(
            "{text}\n[patch.crates-io]\nlocal-patch = {{ path = '{}' }}\n",
            dependency.display()
        ),
    )
    .unwrap();
    let manifest = workspace.join("crates/tests-mixed/Cargo.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        format!("{text}\n[dependencies]\nlocal-patch = \"0.1.0\"\n"),
    )
    .unwrap();
    std::fs::write(
        workspace.join("crates/tests-mixed/src/lib.rs"),
        "pub const VALUE: u8 = local_patch::VALUE;\n",
    )
    .unwrap();
    assert_success(
        &fixture
            .cargo(["generate-lockfile", "--offline"])
            .output()
            .unwrap(),
        "patch lockfile",
    );
    git(&workspace, &["init", "-q"]);
    commit(&workspace);
    let check = || {
        fixture
            .heli(["check", "-p", "tests-mixed"])
            .output()
            .unwrap()
    };
    assert_success(&check(), "initial patched check");
    let hit = check();
    assert!(
        String::from_utf8_lossy(&hit.stderr).contains("already passed"),
        "{hit:?}"
    );
    std::fs::write(&source, "compile_error!(\"changed patched source\");\n").unwrap();
    let changed = check();
    assert_eq!(
        changed.status.code(),
        Some(101),
        "a patch change returned a false green: {changed:?}"
    );
}

#[test]
fn a_command_line_configuration_file_is_keyed_by_content() {
    let fixture = Fixture::new("basic");
    let workspace = fixture.workspace();
    std::fs::write(
        workspace.join("crates/tests-mixed/src/lib.rs"),
        "#[cfg(broken_input)] compile_error!(\"changed external configuration\");\n",
    )
    .unwrap();
    let config = fixture.path("selected-config.toml");
    std::fs::write(&config, "[build]\nrustflags = []\n").unwrap();
    assert_success(
        &fixture
            .cargo(["generate-lockfile", "--offline"])
            .output()
            .unwrap(),
        "lockfile",
    );
    git(&workspace, &["init", "-q"]);
    commit(&workspace);
    let check = || {
        fixture
            .heli(["check", "-p", "tests-mixed", "--config"])
            .arg(&config)
            .output()
            .unwrap()
    };
    assert_success(&check(), "first configured check");
    let hit = check();
    assert!(
        String::from_utf8_lossy(&hit.stderr).contains("already passed"),
        "{hit:?}"
    );
    std::fs::write(&config, "[build]\nrustflags = ['--cfg=broken_input']\n").unwrap();
    let changed = check();
    assert_eq!(
        changed.status.code(),
        Some(101),
        "changed config file returned a false green: {changed:?}"
    );
}

#[test]
fn a_dirty_gitlink_dependency_cannot_reuse_a_passing_check() {
    let fixture = Fixture::new("basic");
    let workspace = fixture.workspace();
    let dependency = workspace.join("subdep");
    std::fs::create_dir_all(dependency.join("src")).unwrap();
    std::fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"audit-subdep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(dependency.join("src/lib.rs"), "pub const VALUE: u8 = 1;\n").unwrap();
    git(&dependency, &["init", "-q"]);
    commit(&dependency);
    let manifest = workspace.join("Cargo.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    text.push_str("\nexclude = [\"subdep\"]\n");
    std::fs::write(manifest, text).unwrap();
    let package = workspace.join("crates/tests-mixed/Cargo.toml");
    let mut text = std::fs::read_to_string(&package).unwrap();
    text.push_str("\n[dependencies]\naudit-subdep = { path = \"../../subdep\" }\n");
    std::fs::write(package, text).unwrap();
    std::fs::write(
        workspace.join("crates/tests-mixed/src/lib.rs"),
        "pub const VALUE: u8 = audit_subdep::VALUE;\n",
    )
    .unwrap();
    let lock = fixture
        .cargo(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert_success(&lock, "generate fixture lockfile");
    git(&workspace, &["init", "-q"]);
    commit(&workspace);
    let first = fixture
        .heli(["check", "-p", "tests-mixed"])
        .output()
        .unwrap();
    assert_success(&first, "first check");
    std::fs::write(
        dependency.join("src/lib.rs"),
        "compile_error!(\"audit dirty dependency\");\n",
    )
    .unwrap();
    let second = fixture
        .heli(["check", "-p", "tests-mixed"])
        .output()
        .unwrap();
    let forced = fixture
        .heli(["--no-cache", "check", "-p", "tests-mixed"])
        .output()
        .unwrap();
    assert_eq!(
        forced.status.code(),
        Some(101),
        "the changed dependency must fail: {forced:?}"
    );
    assert_eq!(
        second.status.code(),
        Some(101),
        "a changed non-ignored dependency returned a false green: {second:?}"
    );
}

#[test]
fn an_ident_filtered_source_cannot_reuse_a_passing_check() {
    let fixture = Fixture::new("basic");
    let workspace = fixture.workspace();
    std::fs::write(
        workspace.join(".gitattributes"),
        "crates/tests-mixed/src/data.txt ident\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("crates/tests-mixed/src/lib.rs"),
        "const _: [(); 1] = [(); (include_bytes!(\"data.txt\")[5] == b'a') as usize];\n",
    )
    .unwrap();
    let data = workspace.join("crates/tests-mixed/src/data.txt");
    std::fs::write(&data, "$Id: a $\n").unwrap();
    let lock = fixture
        .cargo(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert_success(&lock, "generate fixture lockfile");
    git(&workspace, &["init", "-q"]);
    commit(&workspace);
    let first = fixture
        .heli(["check", "-p", "tests-mixed"])
        .output()
        .unwrap();
    assert_success(&first, "first check");
    std::fs::write(&data, "$Id: b $\n").unwrap();
    let second = fixture
        .heli(["check", "-p", "tests-mixed"])
        .output()
        .unwrap();
    let forced = fixture
        .heli(["--no-cache", "check", "-p", "tests-mixed"])
        .output()
        .unwrap();
    assert_eq!(
        forced.status.code(),
        Some(101),
        "raw source change must fail: {forced:?}"
    );
    assert_eq!(
        second.status.code(),
        Some(101),
        "Git ident filtering hid changed compiler input and returned a false green: {second:?}"
    );
}

#[test]
fn captured_failure_json_with_non_cargo_reason_is_preserved() {
    let fixture = Fixture::new("basic");
    let source = r#"
#[test]
fn json_failure() {
    println!("{{\"reason\":\"application-error\",\"detail\":\"audit-sentinel\"}}");
    panic!("intentional audit failure");
}
"#;
    std::fs::write(
        fixture.workspace().join("crates/tests-mixed/src/lib.rs"),
        source,
    )
    .unwrap();
    let output = fixture
        .heli(["--output", "compact", "test", "-p", "tests-mixed"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("audit-sentinel"),
        "captured failure output lost: {stdout}"
    );
}

#[test]
fn log_errors_preserves_colored_errors_from_full_mode() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let writes = fixture.path("audit-writes");
    std::fs::write(&writes,
        "err \u{1b}[1;31merror[E0308]\u{1b}[0m: audit-sentinel\\n\nerr   --> src/lib.rs:1:1\\n\nerr \\n\n"
    ).unwrap();
    let output = fixture
        .heli(["--output", "full", "build"])
        .env("CARGO", echo)
        .env("ECHO_WRITES", writes)
        .env("ECHO_EXIT", "101")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("audit-sentinel"));
    let (_, footer) = split_footer(&output.stderr);
    let logged = fixture
        .heli(["log", footer.run.as_str(), "--errors"])
        .output()
        .unwrap();
    assert_success(&logged, "cargo heli log --errors");
    assert!(
        String::from_utf8_lossy(&logged.stderr).contains("audit-sentinel"),
        "colored compiler error was lost: {logged:?}"
    );
}
