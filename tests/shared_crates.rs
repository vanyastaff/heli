//! Shared crates (docs/build-store.md#shared-crates, #139): with `[store]
//! shared_crates = true`, a dependency that is not a local package is
//! compiled once per store, and every build directory links the same files.
//!
//! The dependency is vendored (a Cargo directory source), so that the tests
//! build offline; cargo treats it as it treats a registry crate. It has a
//! build script that generates code into `OUT_DIR` and sets a variable for
//! the compiler, the inputs that differ between build directories.

mod support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};
use support::{Fixture, assert_success};

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create a directory");
    std::fs::write(path, bytes).expect("write a file");
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("a JSON string")
}

/// A vendored package `heli-shared-dep` with a build script, as a Cargo
/// directory source in `<fixture>/vendor`.
fn vendor(fixture: &Fixture) -> PathBuf {
    let root = fixture.path("vendor");
    let package = root.join("heli-shared-dep");
    let files = [
        (
            "Cargo.toml",
            "[package]\nname='heli-shared-dep'\nversion='1.0.0'\nedition='2021'\nbuild='build.rs'\n",
        ),
        (
            "build.rs",
            "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
             std::fs::write(format!(\"{out}/generated.rs\"), \"pub const GENERATED: u32 = 5;\").unwrap();\n    \
             println!(\"cargo::rustc-env=HELI_SHARED_DEP=7\");\n}\n",
        ),
        (
            "src/lib.rs",
            "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n\
             pub fn value() -> u32 { GENERATED + env!(\"HELI_SHARED_DEP\").parse::<u32>().unwrap() }\n",
        ),
    ];
    let mut checksums = serde_json::Map::new();
    for (name, text) in files {
        write(&package.join(name), text);
        checksums.insert(
            name.to_owned(),
            format!("{:x}", Sha256::digest(text.as_bytes())).into(),
        );
    }
    write(
        &package.join(".cargo-checksum.json"),
        serde_json::json!({
            "files": checksums,
            "package": "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"
        })
        .to_string(),
    );
    root
}

/// A project `name` that depends on the vendored package, with its own
/// repository, building into the store.
fn project(fixture: &Fixture, name: &str, vendor: &Path) -> PathBuf {
    let root = fixture.path(name);
    write(
        &root.join("Cargo.toml"),
        "[workspace]\n[package]\nname='consumer'\nversion='0.1.0'\nedition='2021'\n\
         [dependencies]\nheli-shared-dep='=1.0.0'\n",
    );
    write(
        &root.join("src/lib.rs"),
        "pub fn value() -> u32 { heli_shared_dep::value() }\n\
         #[test] fn reads_the_dependency() { assert_eq!(value(), 12); }\n",
    );
    let build_dir = fixture.path("store").join("{workspace-path-hash}");
    write(
        &root.join(".cargo/config.toml"),
        format!(
            "[source.crates-io]\nreplace-with='vendored'\n[source.vendored]\ndirectory={}\n\
             [build]\nbuild-dir={}\n",
            quoted(vendor),
            quoted(&build_dir)
        ),
    );
    let git = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .output()
        .expect("run git init");
    assert_success(&git, "git init");
    root
}

fn heli_in(fixture: &Fixture, dir: &Path, args: &[&str]) {
    let output = fixture
        .heli(args.iter().copied())
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run heli");
    assert_success(&output, &format!("cargo heli {}", args.join(" ")));
}

/// The entries of the store's shared crates, each with its record.
fn entries(store: &Path) -> Vec<(PathBuf, Value)> {
    let mut entries = Vec::new();
    let Ok(outer) = std::fs::read_dir(store.join(".crates")) else {
        return entries;
    };
    for outer in outer.filter_map(Result::ok) {
        let Ok(inner) = std::fs::read_dir(outer.path()) else {
            continue;
        };
        for entry in inner.filter_map(Result::ok) {
            if let Ok(bytes) = std::fs::read(entry.path().join("meta.json")) {
                entries.push((
                    entry.path(),
                    serde_json::from_slice(&bytes).expect("a record"),
                ));
            }
        }
    }
    entries
}

/// The files in the build directories of `store` named `name`, a prefix.
fn built(store: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![store.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".crates" || name == ".trash" {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            } else if name.starts_with(prefix) && name.ends_with(".rlib") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Two projects build the same vendored crate: it is compiled once, and
/// both build directories link the entry's files; their tests pass; a
/// build again recompiles nothing; `gc` lists the entry, linked.
#[test]
fn a_dependency_is_compiled_once_and_linked_into_every_build_directory() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("store");
    fixture.configure(&format!(
        "[store]\npaths = ['{}']\nshared_crates = true\n",
        store.display()
    ));
    let vendor = vendor(&fixture);
    let first = project(&fixture, "first", &vendor);
    let second = project(&fixture, "second", &vendor);

    heli_in(&fixture, &first, &["build", "--quiet"]);
    heli_in(&fixture, &second, &["build", "--quiet"]);

    let shared: Vec<(PathBuf, Value)> = entries(&store)
        .into_iter()
        .filter(|(_, meta)| meta["crate_name"] == "heli_shared_dep")
        .collect();
    assert_eq!(shared.len(), 1, "one entry for the library: {shared:?}");
    let rlibs = built(&store, "libheli_shared_dep-");
    assert_eq!(rlibs.len(), 2, "{rlibs:?}");
    assert_eq!(
        std::fs::read(&rlibs[0]).expect("read"),
        std::fs::read(&rlibs[1]).expect("read")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let first = std::fs::metadata(&rlibs[0]).expect("metadata");
        let second = std::fs::metadata(&rlibs[1]).expect("metadata");
        assert_eq!(first.ino(), second.ino(), "one file, linked twice");
        assert_eq!(first.nlink(), 3, "the entry's and two build directories'");
    }
    // The consumers' crates are their own.
    assert_eq!(built(&store, "libconsumer-").len(), 2);

    heli_in(&fixture, &first, &["test", "--quiet"]);
    heli_in(&fixture, &second, &["test", "--quiet"]);

    // Built again, nothing is compiled: the linked files keep their times,
    // and cargo finds each unit fresh.
    let consumers = built(&store, "libconsumer-");
    let times: Vec<_> = consumers
        .iter()
        .map(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .expect("a time")
        })
        .collect();
    heli_in(&fixture, &first, &["build", "--quiet"]);
    heli_in(&fixture, &second, &["build", "--quiet"]);
    let again: Vec<_> = consumers
        .iter()
        .map(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .expect("a time")
        })
        .collect();
    assert_eq!(times, again, "a build again recompiled the consumers");

    let output = fixture
        .heli(["gc", "--dry-run", "--json"])
        .output()
        .expect("run heli gc");
    assert_success(&output, "cargo heli gc --dry-run");
    let listing: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let entry = spelled(&shared[0].0);
    let listed = listing["dirs"]
        .as_array()
        .expect("dirs")
        .iter()
        .find(|dir| spelled(Path::new(dir["path"].as_str().expect("a path"))) == entry)
        .unwrap_or_else(|| panic!("the entry is not listed: {listing}"))
        .clone();
    assert_eq!(listed["kind"], "crate", "{listing}");
    assert_eq!(listed["state"], "live", "{listing}");
}

/// Without the setting, nothing is shared.
#[test]
fn without_the_setting_nothing_is_shared() {
    let fixture = Fixture::new("basic");
    let store = fixture.path("store");
    fixture.configure(&format!("[store]\npaths = ['{}']\n", store.display()));
    let vendor = vendor(&fixture);
    let first = project(&fixture, "first", &vendor);
    heli_in(&fixture, &first, &["build", "--quiet"]);
    assert_eq!(entries(&store).len(), 0);
    assert_eq!(built(&store, "libheli_shared_dep-").len(), 1);
}

fn spelled(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
