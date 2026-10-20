//! Since when an orphan is unclaimed: `.heli/orphaned` in a build
//! directory, or in an isolated Cargo home (`super::home`), which heli
//! writes when an eviction pass first finds the directory an orphan (its
//! workspace is gone):
//!
//! ```json
//! {"workspace":"D:\\flui\\.claude\\worktrees\\agent-a64b","since":1791911112}
//! ```
//!
//! The ladder's tier 2 takes an orphan only once it has been unclaimed for
//! 24 h (`super::ladder`), so recycling (docs/build-store.md#recycling) has
//! that long to claim it. The note counts only while it names the workspace of
//! the owner manifest and is no older than its last run: a recycled directory
//! names its new workspace, and a worktree that came back ran since. A
//! directory found live loses its note. Without a note that counts, an orphan
//! is unclaimed since now, so a pass that cannot write (`gc --dry-run`) never
//! makes one evictable. Times are whole seconds since 1970.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{Dir, State};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    workspace: PathBuf,
    since: u64,
}

/// The note's path in `build_dir`.
fn path(build_dir: &Path) -> PathBuf {
    build_dir.join(".heli").join("orphaned")
}

/// Since when the orphan `dir` is unclaimed, by its note, if one counts.
pub fn since(dir: &Dir) -> Option<u64> {
    let owner = dir.owner.as_ref()?;
    let text = std::fs::read(path(&dir.path)).ok()?;
    let note: Note = serde_json::from_slice(&text).ok()?;
    (note.workspace == owner.workspace && note.since >= owner.last_run).then_some(note.since)
}

/// Brings the note of build directory `dir`, in `state`, up to date at
/// `now`: an orphan without a note that counts gets one, and a live
/// directory loses its note. Returns since when an orphan is unclaimed.
pub fn note(dir: &Dir, state: State, now: u64) -> io::Result<Option<u64>> {
    match state {
        State::Live => match std::fs::remove_file(path(&dir.path)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(None),
        },
        State::Orphan => {
            if let Some(since) = since(dir) {
                return Ok(Some(since));
            }
            let Some(owner) = &dir.owner else {
                return Ok(None);
            };
            write(
                &dir.path,
                &Note {
                    workspace: owner.workspace.clone(),
                    since: now,
                },
            )?;
            Ok(Some(now))
        }
        State::Busy | State::Unknown => Ok(since(dir)),
    }
}

/// Brings the notes of the build directories and homes among `dirs` up to
/// date at `now`, by whether their workspace exists alone (no lock is
/// tried).
pub fn keep(dirs: &[Dir], now: u64) {
    for dir in dirs {
        let (super::Kind::Build | super::Kind::Home, Some(owner)) = (dir.kind, &dir.owner) else {
            continue;
        };
        let state = if dir.kind.workspace_exists(&owner.workspace) {
            State::Live
        } else {
            State::Orphan
        };
        let _ = note(dir, state, now);
    }
}

/// Replaces the note in `build_dir` whole, so a reader never sees half.
fn write(build_dir: &Path, note: &Note) -> io::Result<()> {
    let text = serde_json::to_vec(note).map_err(io::Error::other)?;
    let path = path(build_dir);
    let dir = path.parent().expect("BUG: the note is in .heli/");
    std::fs::create_dir_all(dir)?;
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let temporary = dir.join(format!(
        "orphaned.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}
