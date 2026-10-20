//! Isolated Cargo homes (docs/build-store.md#worktree-cargo-homes) as
//! directories the store accounts for (#120): `<state>/h/<32 hex digits>`,
//! one per worktree, published by `crate::dependency_home` and never
//! rewritten. Each holds its worktree's registry index, `.crate` files,
//! extracted sources and git checkouts, gigabytes and a million files on a
//! host whose agents make and drop worktrees all day. A worktree removed
//! leaves its home behind, and nothing but the store ever removes one.
//!
//! **Whose it is.** A home's identity hashes its worktree's path, among
//! other things, but its marker does not spell the path out, so the client
//! that selects a home for a run records the worktree in the home's owner
//! manifest, `.heli/owner` ([`record_use`], `super::owner`), with the run's
//! time as its last run: the manifest a build directory has, so the home is
//! live or an orphan as one is ([`super::Kind::workspace_exists`]: a
//! worktree exists while its `.git` entry does, or, outside any repository,
//! its `Cargo.toml`). A home no run selected since that was recorded, one an
//! older heli published, has no manifest and is **unknown**: nothing heli
//! knows selects it.
//!
//! **Busy.** Cargo holds `.package-cache-mutate` in its home, shared, for
//! the whole of a build, and `.package-cache` exclusively while it
//! downloads (verified with cargo 1.99), so a home whose locks cannot be
//! taken is busy ([`LOCKS`], [`busy`]), as is one whose worktree a run is in
//! flight in (`super::lease`).
//!
//! **Rungs** (`super::kind`): tier 2 for the home of a removed worktree
//! unclaimed for `orphan_hours`, and for an unknown home idle that long;
//! tier 5 for the home of a live worktree idle for `dir_hours`. A home is
//! evicted whole, into [`trash`] beside it, on its volume; a worktree that
//! runs again gets a new one, and downloads again. A home counts toward the
//! budget like any directory, and `status` and `gc` list it as `home`.

use std::io;
use std::path::{Path, PathBuf};

pub use crate::dependency_home::MARKER;

/// The directory of the homes, in the state directory.
pub const BASE: &str = "h";

/// The lock files cargo holds in its home: `.package-cache-mutate`, shared,
/// for the whole of a build, and `.package-cache`, exclusively, while it
/// downloads (`src/cargo/util/cache_lock.rs`).
pub const LOCKS: [&str; 2] = [".package-cache-mutate", ".package-cache"];

/// The homes' trash, beside them.
pub const TRASH: &str = ".heli-trash";

/// Where the homes are, under `state_dir`.
pub fn base(state_dir: &Path) -> PathBuf {
    state_dir.join(BASE)
}

/// The trash of the homes under `state_dir`.
pub fn trash(state_dir: &Path) -> PathBuf {
    base(state_dir).join(TRASH)
}

/// The homes under `state_dir`, in order: the directories of [`base`]
/// named by 32 hex digits that hold a marker, never through a link.
/// Anything else there (the trash, a publication being staged) is not one.
pub fn find(state_dir: &Path) -> Vec<PathBuf> {
    find_in(&base(state_dir))
}

/// The homes in `base`, in order.
fn find_in(base: &Path) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = super::subdirs(base)
        .into_iter()
        .filter(|dir| {
            super::is_hex(dir, Some(32))
                && std::fs::symlink_metadata(dir.join(MARKER)).is_ok_and(|marker| marker.is_file())
        })
        .collect();
    homes.sort();
    homes
}

/// The paths of cargo's [`LOCKS`] in `home`, whether they exist.
pub fn locks(home: &Path) -> Vec<PathBuf> {
    LOCKS.iter().map(|name| home.join(name)).collect()
}

/// Whether a cargo holds one of the [`LOCKS`] in `home`: it cannot be
/// taken. heli only tries them, and lets go at once.
pub fn busy(home: &Path) -> bool {
    locks(home)
        .iter()
        .any(|lock| matches!(super::try_lock(lock), super::Lock::Held))
}

/// Records that a run in `worktree` that started `at` selected `home`: the
/// owner manifest is created, or its worktree and last run updated. A home
/// belongs to a worktree, not to a repository, so the manifest names none.
pub fn record_use(home: &Path, worktree: &Path, at: u64) -> io::Result<()> {
    super::owner::record_run(home, worktree, None, at)
}

/// Whether the worktree `worktree` still exists: its `.git` entry (a
/// directory in a main checkout, a file in a linked one), since a removed
/// worktree can leave files behind; outside any repository, where the
/// worktree is the directory the run started in, its `Cargo.toml`.
pub fn worktree_exists(worktree: &Path) -> bool {
    worktree.join(".git").exists() || worktree.join("Cargo.toml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homes_are_found_by_name_and_marker() {
        let dir = tempfile::tempdir().expect("dir");
        let home = "0123456789abcdef0123456789abcdef";
        let marked = ["fedcba9876543210fedcba9876543210", home];
        let unmarked = "00000000000000000000000000000000";
        let names = marked
            .iter()
            .chain([&unmarked, &".stage-1-2-3", &TRASH, &"0123456789abcdef"]);
        for name in names {
            std::fs::create_dir_all(dir.path().join(name)).expect("dir");
        }
        for name in marked.iter().chain([&".stage-1-2-3", &"0123456789abcdef"]) {
            std::fs::write(dir.path().join(name).join(MARKER), "{}").expect("marker");
        }
        std::fs::write(dir.path().join(unmarked).join("config.toml"), "").expect("file");
        let found: Vec<String> = find_in(dir.path())
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, [home, marked[0]]);
        assert_eq!(trash(dir.path()), dir.path().join(BASE).join(TRASH));
        assert_eq!(
            locks(Path::new("home")),
            [
                Path::new("home").join(".package-cache-mutate"),
                Path::new("home").join(".package-cache")
            ]
        );
    }

    #[test]
    fn a_worktree_exists_by_its_git_entry_or_manifest() {
        let dir = tempfile::tempdir().expect("dir");
        let linked = dir.path().join("linked");
        std::fs::create_dir_all(&linked).expect("dir");
        assert!(
            !worktree_exists(&linked),
            "files left behind are no worktree"
        );
        std::fs::write(linked.join(".git"), "gitdir: elsewhere").expect("git file");
        assert!(worktree_exists(&linked));
        let bare = dir.path().join("bare");
        std::fs::create_dir_all(&bare).expect("dir");
        std::fs::write(bare.join("Cargo.toml"), "").expect("manifest");
        assert!(
            worktree_exists(&bare),
            "outside a repository, the directory run in"
        );
        assert!(!worktree_exists(&dir.path().join("gone")));
    }
}
