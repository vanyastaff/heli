//! Temporary directories for tests that run cargo, shared by the unit tests
//! and (through `#[path]`) the integration tests' harness.
//!
//! Cargo reads the `.cargo/config.toml` of every ancestor of its working
//! directory. A test workspace in the system's temporary directory, under
//! the user's home on Windows, therefore builds with the user's own Cargo
//! configuration: a user-wide `build-dir` sent every test workspace into the
//! user's real build store. And a test workspace inside a git repository is
//! in that repository: `cargo heli setup` there offers to edit it. Tests
//! make their directories under [`root`] instead, which has neither among
//! its ancestors, and [`tempdir`] fails when it has.

use std::path::{Path, PathBuf};

/// Overrides [`root`].
pub const VAR: &str = "HELI_TEST_TMP";

/// Where tests make their temporary directories: [`VAR`] if set, else the
/// first of the system's temporary directory and `tmp` in the build
/// directory the test binary is in (`CARGO_TARGET_TMPDIR`) with no Cargo
/// configuration or repository among its ancestors, else the former.
pub fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os(VAR).filter(|dir| !dir.is_empty()) {
        let dir = PathBuf::from(dir);
        return std::path::absolute(&dir)
            .unwrap_or_else(|error| panic!("{VAR} {}: {error}", dir.display()));
    }
    let temp = std::env::temp_dir();
    let build = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.ancestors().nth(3)?.join("tmp")));
    std::iter::once(temp.clone())
        .chain(build)
        .find(|dir| hazard(dir).is_none())
        .unwrap_or(temp)
}

/// A new temporary directory in [`root`], removed on drop.
///
/// # Panics
///
/// If a `.cargo/config.toml`, a `.cargo/config` or a `.git` is in [`root`]
/// or any of its ancestors: every cargo a test ran there would read the
/// configuration, and heli would take the repository for the test's.
pub fn tempdir() -> tempfile::TempDir {
    let root = root();
    if let Some(file) = hazard(&root) {
        panic!(
            "{} is above {}, where the tests make their workspaces; \
             set {VAR} to a directory with no Cargo configuration or git repository among its ancestors",
            file.display(),
            root.display(),
        );
    }
    std::fs::create_dir_all(&root)
        .unwrap_or_else(|error| panic!("create {}: {error}", root.display()));
    tempfile::tempdir_in(&root)
        .unwrap_or_else(|error| panic!("temporary directory in {}: {error}", root.display()))
}

/// The first Cargo configuration or repository among `dir` and its
/// ancestors.
fn hazard(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find_map(|dir| {
        [
            dir.join(".cargo").join("config.toml"),
            dir.join(".cargo").join("config"),
            dir.join(".git"),
        ]
        .into_iter()
        .find(|file| file.exists())
    })
}
