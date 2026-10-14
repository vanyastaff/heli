//! The registered `target/` directories, kept across daemon restarts in
//! `store.json` in the state directory:
//!
//! ```json
//! {"targets":[{"path":"D:\\flui\\target","workspace":"D:\\flui"}]}
//! ```
//!
//! A `target/` directory is registered, with the workspace it was last seen
//! for, when a heli run builds in that workspace (`crate::daemon::accounts`).
//! The daemon adds to it; `gc --dry-run` and the daemon's accounts read it.
//! It is replaced whole, so a reader never sees half of it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::same_path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    pub targets: Vec<Target>,
}

/// A registered `target/` directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub path: PathBuf,
    /// The workspace it was last seen for.
    pub workspace: PathBuf,
}

/// The registry's file in `state_dir`.
fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("store.json")
}

impl Registry {
    /// The registry in `state_dir`; none there, or one that cannot be read,
    /// is empty.
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read(path(state_dir))
            .ok()
            .and_then(|text| serde_json::from_slice(&text).ok())
            .unwrap_or_default()
    }

    /// Registers `target` in the registry in `state_dir`, or updates its
    /// workspace. Only one writer may call this at a time.
    pub fn add(state_dir: &Path, target: Target) -> io::Result<()> {
        let mut registry = Self::load(state_dir);
        let known = registry
            .targets
            .iter_mut()
            .find(|known| same_path(&known.path, &target.path));
        match known {
            Some(known) if known.workspace == target.workspace => return Ok(()),
            Some(known) => known.workspace = target.workspace,
            None => registry.targets.push(target),
        }
        registry.save(state_dir)
    }

    fn save(&self, state_dir: &Path) -> io::Result<()> {
        static WRITES: AtomicU64 = AtomicU64::new(0);
        let text = serde_json::to_vec(self).map_err(io::Error::other)?;
        let temporary = state_dir.join(format!(
            "store.json.{}.{}.tmp",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path(state_dir)).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
    }
}
