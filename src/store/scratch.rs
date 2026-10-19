//! Scratch copies that cargo tools leave outside the store (
//! docs/build-store.md#eviction, tier 0): cargo-mutants copies the workspace
//! into the system's temporary directory for its builds (`mutants-*` on
//! Windows, to keep paths short, `cargo-mutants-<source directory>-*`
//! elsewhere) and deletes each copy when it is done with it. A run that is
//! cancelled or killed leaves its copies behind, each a whole source tree with
//! a `target/` of its own: the disk fills with no workspace that will ever use
//! them.
//!
//! [`find`] lists them: directories of [`root`] with one of those names that
//! hold a `Cargo.toml`. The ladder evicts one once nothing in it was written
//! for `[store] scratch_hours` and no cargo holds a lock inside it, first of
//! all. Its trash is [`trash`], in the same directory, so on its volume.

use std::path::{Path, PathBuf};

/// Overrides where scratch copies are looked for; tests point it at a
/// directory of their own, so they never touch the machine's.
pub const VAR: &str = "HELI_SCRATCH_DIR";

/// Where the tools put their scratch copies: [`VAR`] if set, else the
/// system's temporary directory.
pub fn root() -> PathBuf {
    std::env::var_os(VAR)
        .filter(|dir| !dir.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

/// The scratch copies in [`root`], in order.
pub fn find() -> Vec<PathBuf> {
    find_in(&root())
}

/// The scratch copies in `root`, in order.
fn find_in(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = super::subdirs(root)
        .into_iter()
        .filter(|dir| {
            dir.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_scratch_name)
                && dir.join("Cargo.toml").is_file()
        })
        .collect();
    found.sort();
    found
}

/// The trash of the scratch copies, beside them.
pub fn trash() -> PathBuf {
    root().join(".heli-trash")
}

/// Whether `name` is one cargo-mutants gives its copies.
fn is_scratch_name(name: &str) -> bool {
    name.starts_with("mutants-") || name.starts_with("cargo-mutants-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_are_found_by_name_and_manifest() {
        let dir = tempfile::tempdir().expect("dir");
        for name in [
            "mutants-a1b2",
            "cargo-mutants-app-x9",
            "mutants-empty",
            "other-x",
        ] {
            std::fs::create_dir_all(dir.path().join(name)).expect("dir");
        }
        for name in ["mutants-a1b2", "cargo-mutants-app-x9", "other-x"] {
            std::fs::write(dir.path().join(name).join("Cargo.toml"), "").expect("manifest");
        }
        std::fs::write(dir.path().join("mutants-file"), "").expect("file");
        let names: Vec<String> = find_in(dir.path())
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["cargo-mutants-app-x9", "mutants-a1b2"]);
        assert_eq!(trash(), root().join(".heli-trash"));
    }
}
