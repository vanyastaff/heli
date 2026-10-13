//! The worktree a run belongs to.

use std::path::{Path, PathBuf};

/// The worktree that contains `dir`: the nearest directory, `dir` itself or
/// above it, that holds a `.git` entry (a directory in a main checkout, a
/// file in a linked worktree). Outside any repository it is `dir` itself.
pub fn of(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}
