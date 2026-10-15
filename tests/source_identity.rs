//! Precise recycling uses the bytes rustc reads, and accounts for CLI targets.
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{Fixture, assert_success};

#[test]
fn a_command_line_target_directory_is_registered() {
    let fixture = Fixture::new("basic");
    let actual = fixture.path("explicit-target");
    let output = fixture
        .heli(["build", "--quiet", "-p", "echo-args", "--target-dir"])
        .arg(&actual)
        .output()
        .expect("run public build with a target-dir override");
    assert_success(&output, "build with --target-dir");
    assert!(actual.join("debug").is_dir(), "Cargo used the override");
    let path = fixture.path("state").join("store.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    let registry = loop {
        if let Ok(bytes) = std::fs::read(&path) {
            break serde_json::from_slice::<Value>(&bytes).expect("parse registry");
        }
        assert!(
            Instant::now() < deadline,
            "no registry at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        registry["targets"].as_array().unwrap().iter().any(|entry| {
            entry["path"].as_str().is_some_and(|path| {
                std::fs::canonicalize(path).ok() == std::fs::canonicalize(&actual).ok()
            })
        }),
        "the store must account for the actual CLI-selected target directory {}; registry={registry}",
        actual.display(),
    );
}

struct IdentRepo {
    fixture: Fixture,
    main: PathBuf,
    store: PathBuf,
}

impl IdentRepo {
    fn new() -> Self {
        let fixture = Fixture::new("basic");
        let main = fixture.workspace().join("ident-main");
        let store = fixture.path("store");
        fixture.configure(&format!("[store]\npaths = ['{}']\n", store.display()));
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(
            main.join("Cargo.toml"),
            "[workspace]\n[package]\nname = \"audit-ident\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        ).unwrap();
        std::fs::write(
            main.join("src/main.rs"),
            "fn main() { print!(\"{}\", include_str!(\"payload.txt\")); }\n",
        )
        .unwrap();
        std::fs::write(main.join("src/payload.txt"), "$Id: a $").unwrap();
        std::fs::write(main.join(".gitattributes"), "*.txt ident\n").unwrap();
        let cargo = fixture.workspace().join(".cargo");
        std::fs::create_dir_all(&cargo).unwrap();
        std::fs::write(
            cargo.join("config.toml"),
            format!(
                "[build]\nbuild-dir = \"{}\"\n",
                store
                    .join("{workspace-path-hash}")
                    .to_string_lossy()
                    .replace('\\', "\\\\")
            ),
        )
        .unwrap();
        std::fs::write(fixture.path("gitconfig"), "").unwrap();
        let repo = Self {
            fixture,
            main,
            store,
        };
        repo.git(&repo.main, &["init", "--quiet"]);
        repo.git(&repo.main, &["add", "-A"]);
        repo.git(
            &repo.main,
            &[
                "-c",
                "user.name=heli",
                "-c",
                "user.email=heli@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                "fixture",
            ],
        );
        repo
    }

    fn isolated(&self, command: &mut Command) {
        command
            .env_remove("CARGO_TARGET_DIR")
            .env("GIT_CONFIG_GLOBAL", self.fixture.path("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut command = self.fixture.command("git", args.iter().copied());
        command.current_dir(cwd);
        self.isolated(&mut command);
        let output = command.output().expect("run git");
        assert_success(&output, "fixture git");
        output
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.fixture.workspace().join(name);
        self.git(
            &self.main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                name,
                path.to_str().unwrap(),
            ],
        );
        path
    }

    fn heli(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut command = self.fixture.heli(args.iter().copied());
        command.current_dir(cwd);
        self.isolated(&mut command);
        let output = command.output().expect("run public heli command");
        assert_success(&output, "heli command");
        output
    }

    fn build_dir(&self, workspace: &Path) -> PathBuf {
        for outer in std::fs::read_dir(&self.store)
            .unwrap()
            .filter_map(Result::ok)
        {
            if !outer.file_type().unwrap().is_dir() {
                continue;
            }
            for inner in std::fs::read_dir(outer.path())
                .unwrap()
                .filter_map(Result::ok)
            {
                let path = inner.path();
                let Ok(bytes) = std::fs::read(path.join(".heli/owner")) else {
                    continue;
                };
                let owner: Value = serde_json::from_slice(&bytes).unwrap();
                if owner["workspace"].as_str().is_some_and(|name| {
                    std::fs::canonicalize(name).ok() == std::fs::canonicalize(workspace).ok()
                }) {
                    return path;
                }
            }
        }
        panic!("no owner manifest for {}", workspace.display())
    }
}

#[test]
fn precise_recycling_preserves_raw_bytes_despite_git_ident_normalization() {
    let repo = IdentRepo::new();
    let donor = repo.worktree("ident-donor");
    // Worktree checkout expands ident; deliberately use equal-sized raw
    // expansions whose clean blobs are identical.
    std::fs::write(donor.join("src/payload.txt"), "$Id: a $").unwrap();
    repo.heli(
        &donor,
        &["--output", "full", "build", "--message-format=json"],
    );
    let donor_build = repo.build_dir(&donor);
    assert!(
        donor_build.join(".heli/sources").is_file(),
        "a recorded donor is needed"
    );
    repo.fixture.kill_daemon();
    repo.git(
        &repo.main,
        &["worktree", "remove", "--force", donor.to_str().unwrap()],
    );
    let recipient = repo.worktree("ident-recipient");
    std::fs::write(recipient.join("src/payload.txt"), "$Id: b $").unwrap();
    repo.heli(&recipient, &["recycle"]);
    let events = repo.fixture.events_named("recycle");
    assert_eq!(events.last().unwrap()["mode"], "precise", "{events:?}");
    let build = repo.heli(
        &recipient,
        &["--output", "full", "build", "--message-format=json"],
    );
    let binary = recipient
        .join("target/debug")
        .join(format!("audit-ident{}", std::env::consts::EXE_SUFFIX));
    let run = Command::new(binary)
        .output()
        .expect("execute the recycled product");
    assert_success(&run, "recycled binary");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "$Id: b $",
        "the recipient product must contain its actual source bytes, not the donor's; Cargo messages: {}",
        String::from_utf8_lossy(&build.stdout),
    );
}

#[test]
fn a_legacy_record_without_raw_digests_recycles_conservatively() {
    let repo = IdentRepo::new();
    let donor = repo.worktree("legacy-donor");
    repo.heli(&donor, &["build"]);
    let record = repo.build_dir(&donor).join(".heli/sources");
    let mut sources: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    for file in sources["files"].as_object_mut().unwrap().values_mut() {
        assert!(file.as_object_mut().unwrap().remove("content").is_some());
    }
    std::fs::write(&record, serde_json::to_vec(&sources).unwrap()).unwrap();
    repo.fixture.kill_daemon();
    repo.git(
        &repo.main,
        &["worktree", "remove", "--force", donor.to_str().unwrap()],
    );
    let recipient = repo.worktree("legacy-recipient");
    std::fs::write(recipient.join("src/payload.txt"), "$Id: b $").unwrap();
    repo.heli(&recipient, &["recycle"]);
    let events = repo.fixture.events_named("recycle");
    assert_eq!(events.last().unwrap()["mode"], "conservative", "{events:?}");
    repo.heli(&recipient, &["build"]);
    let binary = recipient
        .join("target/debug")
        .join(format!("audit-ident{}", std::env::consts::EXE_SUFFIX));
    let output = Command::new(binary)
        .output()
        .expect("execute migrated product");
    assert_success(&output, "migrated product");
    assert_eq!(output.stdout, b"$Id: b $");
}
