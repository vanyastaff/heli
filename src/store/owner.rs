//! The owner manifest, `.heli/owner` in each build directory: whose it is.
//!
//! ```json
//! {"workspace":"D:\\flui\\.claude\\worktrees\\agent-a64b",
//!  "repository":"D:\\flui\\.git","created_at":1791910980,"last_run":1791911112}
//! ```
//!
//! - `workspace`: the workspace root, as cargo names it;
//! - `repository`: the repository's git common directory, the same for every
//!   worktree of one repository; `null` outside a repository;
//! - `created_at`: when heli first wrote the manifest;
//! - `last_run`: when the last heli run in the workspace started.
//!
//! Times are whole seconds since 1970 (UTC). The daemon writes it as a run
//! starts; it replaces the file whole, so a reader never sees half of it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub workspace: PathBuf,
    pub repository: Option<PathBuf>,
    pub created_at: u64,
    pub last_run: u64,
}

/// The manifest's path in `build_dir`.
pub fn path(build_dir: &Path) -> PathBuf {
    build_dir.join(".heli").join("owner")
}

/// The manifest of `build_dir`, if it has one heli can read.
pub fn read(build_dir: &Path) -> Option<Owner> {
    let text = std::fs::read(path(build_dir)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Records a run in `workspace` (of `repository`) that started `at`: the
/// manifest is created, or its workspace, repository and last run updated.
pub fn record_run(
    build_dir: &Path,
    workspace: &Path,
    repository: Option<&Path>,
    at: u64,
) -> io::Result<()> {
    let created_at = read(build_dir).map_or(at, |owner| owner.created_at);
    let owner = Owner {
        workspace: workspace.to_path_buf(),
        repository: repository.map(Path::to_path_buf),
        created_at,
        last_run: at,
    };
    let text = serde_json::to_vec(&owner).map_err(io::Error::other)?;
    let path = path(build_dir);
    let dir = path.parent().expect("BUG: the manifest is in .heli/");
    std::fs::create_dir_all(dir)?;
    // A name of this write's own, so that two writers never share a file.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let temporary = dir.join(format!(
        "owner.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}
