//! The bytes a directory takes, by category: what it is worth to keep.
//!
//! A file's category follows from its path below the directory, in a
//! profile directory (`<profile>/`, or `<target triple>/<profile>/` when a
//! triple, which always holds a `-`, comes first):
//!
//! - `incremental/`: incremental compilation caches;
//! - `deps/`: compiled units;
//! - `build/`: build scripts and their output;
//! - `.fingerprint/`: cargo's freshness records;
//! - tool caches: in a `target/` directory only, the paths the
//!   configuration's patterns name (`[store] tool_caches`) and nothing else;
//!   a pattern's components match the path's first ones, `*` and `?`
//!   within a component;
//! - other: the rest (final artifacts, `doc/`, cargo's bookkeeping).
//!
//! Sizes are file lengths. Links are never followed, so a symlink or
//! junction counts nothing; a hard link counts wherever it is, so a final
//! artifact cargo hardlinked out of the build directory counts in both.

use std::ops::AddAssign;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bytes, by category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sizes {
    pub incremental: u64,
    pub deps: u64,
    pub build_scripts: u64,
    pub fingerprints: u64,
    pub tool_caches: u64,
    pub other: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Incremental,
    Deps,
    BuildScripts,
    Fingerprints,
    ToolCaches,
    Other,
}

impl Sizes {
    pub fn total(&self) -> u64 {
        self.incremental
            + self.deps
            + self.build_scripts
            + self.fingerprints
            + self.tool_caches
            + self.other
    }

    fn add(&mut self, category: Category, bytes: u64) {
        let slot = match category {
            Category::Incremental => &mut self.incremental,
            Category::Deps => &mut self.deps,
            Category::BuildScripts => &mut self.build_scripts,
            Category::Fingerprints => &mut self.fingerprints,
            Category::ToolCaches => &mut self.tool_caches,
            Category::Other => &mut self.other,
        };
        *slot += bytes;
    }
}

impl AddAssign for Sizes {
    fn add_assign(&mut self, other: Self) {
        self.incremental += other.incremental;
        self.deps += other.deps;
        self.build_scripts += other.build_scripts;
        self.fingerprints += other.fingerprints;
        self.tool_caches += other.tool_caches;
        self.other += other.other;
    }
}

/// `bytes` for people: `512 B`, `1.5 KiB`, `12.0 MiB`, `148.3 GiB`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The sizes of the files under `root`, by category; `tool_caches` are the
/// patterns that name its tool caches (none for a build directory).
pub fn scan(root: &Path, tool_caches: &[String]) -> Sizes {
    let patterns: Vec<Vec<&str>> = tool_caches
        .iter()
        .map(|pattern| pattern.split('/').filter(|part| !part.is_empty()).collect())
        .collect();
    // Below this depth, a path's category no longer changes.
    let deciding = patterns.iter().map(Vec::len).max().unwrap_or(0).max(3);
    let mut sizes = Sizes::default();
    // Each directory to read, with its path below `root` while that may
    // still decide its category, else the category it has.
    let mut pending: Vec<(PathBuf, Result<Vec<String>, Category>)> =
        vec![(root.to_path_buf(), Ok(Vec::new()))];
    while let Some((dir, place)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // A `target/` directory's trash is on its way out.
            if dir == root && entry.file_name() == super::trash::TARGET_TRASH {
                continue;
            }
            let place = match &place {
                Err(category) => Err(*category),
                Ok(above) => {
                    let mut components = above.clone();
                    components.push(entry.file_name().to_string_lossy().into_owned());
                    match category(&components, &patterns) {
                        Some(category) => Err(category),
                        None if components.len() >= deciding => Err(Category::Other),
                        None => Ok(components),
                    }
                }
            };
            if kind.is_dir() {
                pending.push((entry.path(), place));
            } else if kind.is_file() {
                let bytes = entry.metadata().map_or(0, |metadata| metadata.len());
                sizes.add(place.err().unwrap_or(Category::Other), bytes);
            }
        }
    }
    sizes
}

/// The category of the path whose components below the root are
/// `components`, if they decide it.
fn category(components: &[String], patterns: &[Vec<&str>]) -> Option<Category> {
    let tool_cache = patterns.iter().any(|pattern| {
        pattern.len() <= components.len()
            && pattern
                .iter()
                .zip(components)
                .all(|(pattern, component)| glob(pattern, component))
    });
    if tool_cache {
        return Some(Category::ToolCaches);
    }
    let of = |name: &str| match name {
        "incremental" => Some(Category::Incremental),
        "deps" => Some(Category::Deps),
        "build" => Some(Category::BuildScripts),
        ".fingerprint" => Some(Category::Fingerprints),
        _ => None,
    };
    let triple = components.first().is_some_and(|first| first.contains('-'));
    components
        .get(1)
        .and_then(|name| of(name))
        .or_else(|| triple.then(|| components.get(2).and_then(|name| of(name)))?)
}

/// Whether `text` matches `pattern`: `*` matches any run of characters,
/// `?` any one.
pub fn glob(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    // The positions in `pattern` reachable after each character of `text`.
    let mut reachable = vec![false; pattern.len() + 1];
    reachable[0] = true;
    let close = |reachable: &mut Vec<bool>| {
        for at in 0..pattern.len() {
            if reachable[at] && pattern[at] == '*' {
                reachable[at + 1] = true;
            }
        }
    };
    close(&mut reachable);
    for character in text {
        let mut next = vec![false; pattern.len() + 1];
        for at in 0..pattern.len() {
            if !reachable[at] {
                continue;
            }
            match pattern[at] {
                '*' => next[at] = true,
                '?' => next[at + 1] = true,
                literal if literal == character => next[at + 1] = true,
                _ => {}
            }
        }
        close(&mut next);
        reachable = next;
    }
    reachable[pattern.len()]
}
