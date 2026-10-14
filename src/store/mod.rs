//! The build store (DESIGN.md §6): cargo's intermediates, kept outside the
//! worktrees in one store per volume.
//!
//! **Placement** ([`serving`], [`build_dir`]). `cargo heli setup` sets
//! cargo's `build.build-dir` to `<store>/{workspace-path-hash}`, so each
//! workspace gets a build directory of its own in the store, at
//! `<store>/<2 hex digits>/<14 hex digits>`; `target/` keeps the final
//! artifacts. The store must be on the worktree's volume ([`volume`]): the
//! first one configured there (`[store] paths`), else the volume's default,
//! `<volume>\.heli\build` on Windows. On Unix the volume's root is rarely
//! writable, so the default there is the user's cache,
//! `$XDG_CACHE_HOME/heli/build` (else `~/.cache/heli/build`), when it is on
//! that volume, and `<volume>/.heli/build` otherwise.

pub mod volume;

use std::path::{Path, PathBuf};

use crate::config::StoreConfig;

/// The template cargo replaces with a hash of the workspace's manifest path.
pub const WORKSPACE_PATH_HASH: &str = "{workspace-path-hash}";

/// The store that serves the volume holding `dir`.
pub fn serving(config: &StoreConfig, dir: &Path) -> Result<PathBuf, String> {
    let Some(root) = volume::root(dir) else {
        return Err(format!("cannot tell the volume of {}", dir.display()));
    };
    if let Some(store) = config.paths.iter().find(|store| volume::same(store, &root)) {
        return Ok(store.clone());
    }
    Ok(default_store(&root))
}

/// The `build.build-dir` that places a workspace's build directory in
/// `store`.
pub fn build_dir(store: &Path) -> PathBuf {
    store.join(WORKSPACE_PATH_HASH)
}

#[cfg(windows)]
fn default_store(root: &Path) -> PathBuf {
    root.join(".heli").join("build")
}

#[cfg(unix)]
fn default_store(root: &Path) -> PathBuf {
    let var = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
    };
    let cache = var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")));
    match cache {
        Some(cache) if volume::same(&cache, root) => cache.join("heli").join("build"),
        _ => root.join(".heli").join("build"),
    }
}
