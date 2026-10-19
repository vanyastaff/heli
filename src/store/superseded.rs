//! Superseded intermediates (docs/build-store.md#eviction, #92): what a
//! registered `target/` directory still holds of cargo's intermediates after
//! its workspace moved them to a build directory elsewhere (`build.build-dir`,
//! as `cargo heli setup` sets it).
//!
//! In a registered, non-shared `target/` directory apart from its build
//! directory since a time (`super::registry::Target::separated_since`),
//! these directories are superseded: `deps/`, `incremental/`, `build/` and
//! `.fingerprint/` in each profile directory (`<profile>/`, or
//! `<triple>/<profile>/` where a triple holds a `-`), when nothing in them
//! was written after that time. One cargo still writes to (another
//! configuration, an environment variable) is not. Final artifacts at a
//! profile's top level, `examples/`, the tool caches and the build
//! directory itself, should it be inside, never are.

use std::path::{Path, PathBuf};

use super::kind::TARGET_TRASH;
use super::ladder::{Measure, measure, tool_caches_in};

/// cargo's intermediates in a profile directory.
const INTERMEDIATES: [&str; 4] = ["deps", "incremental", "build", ".fingerprint"];

/// Where a registered `target/` directory's workspace builds instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Separation {
    /// The build directory.
    pub build_dir: PathBuf,
    /// Since when, in seconds since 1970.
    pub since: u64,
}

/// The superseded directories in the `target/` directory `root`, apart from
/// its build directory as `separation` says, each with what it holds, in
/// order; `tool_caches` are the patterns of its tool caches.
pub fn find(
    root: &Path,
    separation: &Separation,
    tool_caches: &[String],
) -> Vec<(PathBuf, Measure)> {
    let caches: Vec<PathBuf> = tool_caches_in(root, tool_caches)
        .into_iter()
        .map(|(cache, _)| cache)
        .collect();
    let mut profiles = Vec::new();
    for profile in super::subdirs(root) {
        if profile.file_name().is_some_and(|name| name == TARGET_TRASH) {
            continue;
        }
        let triple = profile
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains('-'));
        if triple {
            profiles.extend(super::subdirs(&profile));
        }
        profiles.push(profile);
    }
    let mut found = Vec::new();
    for profile in profiles {
        for name in INTERMEDIATES {
            let dir = profile.join(name);
            let spared = dir.starts_with(&separation.build_dir)
                || separation.build_dir.starts_with(&dir)
                || caches.iter().any(|cache| dir.starts_with(cache));
            if spared || !super::is_real_dir(&dir) {
                continue;
            }
            if untouched(&dir, separation.since) {
                let held = measure(&dir);
                found.push((dir, held));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The bytes of the superseded directories [`find`] names.
pub fn bytes(root: &Path, separation: &Separation, tool_caches: &[String]) -> u64 {
    find(root, separation, tool_caches)
        .iter()
        .map(|(_, held)| held.bytes)
        .sum()
}

/// Whether the directory `dir` is still superseded since `since`: nothing
/// in it was written after.
pub fn still(dir: &Path, since: u64) -> bool {
    untouched(dir, since)
}

/// Whether no file under `dir` was written after `since`. Only files count:
/// a directory's own time changes when an entry is added or removed (even
/// an empty `build/` of a workspace without build scripts, on macOS), and
/// an empty directory holds nothing cargo could read.
fn untouched(dir: &Path, since: u64) -> bool {
    newest_file(dir).is_none_or(|newest| newest <= since)
}

/// When a file under `dir` was last written, never through a link; `None`
/// without any file.
fn newest_file(dir: &Path) -> Option<u64> {
    let mut newest = None;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && let Ok(modified) = entry.metadata().and_then(|meta| meta.modified())
            {
                newest = newest.max(Some(crate::clock::unix_seconds(modified)));
            }
        }
    }
    newest
}
