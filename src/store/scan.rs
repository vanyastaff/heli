//! The path scan (DESIGN.md §6.4, "What cargo does not track"): cargo
//! leaves the variables it sets itself out of its freshness tracking
//! (`OUT_DIR`, `CARGO_MANIFEST_DIR`, `CARGO_BIN_EXE_*`,
//! `CARGO_TARGET_TMPDIR`; cargo's `dep_info.rs`), so a unit that compiled
//! one in stays fresh when its build directory moves, and keeps the old
//! path. rustc still records each value it read in its own dep-info, as a
//! line `# env-dep:NAME=value`, with `\`, line feed and carriage return
//! escaped as `\\`, `\n` and `\r` (`escape_dep_env` in rustc; on Windows,
//! `# env-dep:CARGO_MANIFEST_DIR=E:\\heli`). [`invalidate`] deletes the
//! fingerprint of every unit that read a value under one of the old paths,
//! and cargo rebuilds those units.
//!
//! **Where cargo keeps them**, checked against cargo 1.99's layout of a
//! build directory, in each profile directory (`<profile>/`, or
//! `<target triple>/<profile>/`):
//!
//! - rustc's dep-info is `deps/<crate>-<hash>.d`, `examples/<crate>-<hash>.d`,
//!   and for a build script `build/<package>-<hash>/build_script_build-<hash>.d`;
//! - the unit's fingerprint is the directory `.fingerprint/<package>-<hash>/`,
//!   with the same `<hash>` (cargo's `-C extra-filename`). A crate's name has
//!   `_` where its package's has `-`, so only the hash ties the two.
//! - Some units have no hash in their file names: on Windows MSVC the
//!   executables of path packages (cargo leaves the hash out so the PDB's
//!   path stays put), and the dylibs and cdylibs of path packages
//!   everywhere: `deps/<crate>.d`. Their fingerprint directory still has a
//!   hash, and its files are named after the target (`dep-<kind>-<target>`,
//!   `<kind>-<target>`); such a dep-info file stands for every fingerprint
//!   directory with a file whose name ends in its crate's, `-` read as `_`.
//!   Invalidating one unit too many costs only its rebuild.
//!
//! A value is under a path when its components start with the path's, as
//! the platform compares them: on Windows without case, either separator,
//! and without the `\\?\` prefix. The old paths are spelled as cargo spelled
//! them (the workspace as `cargo metadata` named it, the build directory in
//! a store as cargo's `build-dir` names it, or heli would not manage it),
//! so they need no resolving. A value that holds a list of paths
//! (`PATH`-like) is looked at path by path too.
//!
//! Nothing here follows a link out of the build directory.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Deletes, in the build directory `build_dir`, the fingerprint of every
/// unit whose rustc dep-info has an `env-dep` value under one of `old`.
/// Returns the fingerprint directories it deleted.
pub fn invalidate(build_dir: &Path, old: &[PathBuf]) -> Vec<PathBuf> {
    let prefixes: Vec<Vec<String>> = old
        .iter()
        .map(|path| components(&path.to_string_lossy()))
        .collect();
    let mut deleted = Vec::new();
    for profile in profiles(build_dir) {
        let fingerprints = profile.join(".fingerprint");
        let mut stale = BTreeSet::new();
        let units = fingerprint_dirs(&fingerprints);
        for dep_info in dep_info_files(&profile) {
            if !names_old_path(&dep_info, &prefixes) {
                continue;
            }
            let Some(stem) = dep_info.file_stem().and_then(OsStr::to_str) else {
                continue;
            };
            stale.extend(units_of(stem, &units));
        }
        for name in stale {
            let dir = fingerprints.join(name);
            if std::fs::remove_dir_all(&dir).is_ok() {
                deleted.push(dir);
            }
        }
    }
    deleted
}

/// Deletes, in the build directory `build_dir`, the fingerprint of every
/// unit, so that cargo rebuilds them all. Returns the fingerprint
/// directories it deleted.
pub fn invalidate_all(build_dir: &Path) -> Vec<PathBuf> {
    let mut deleted = Vec::new();
    for profile in profiles(build_dir) {
        for dir in subdirs(&profile.join(".fingerprint")) {
            if std::fs::remove_dir_all(&dir).is_ok() {
                deleted.push(dir);
            }
        }
    }
    deleted
}

/// The components of the path `text`, as the platform compares them: on
/// Windows without case, with either separator, and without `\\?\`.
fn components(text: &str) -> Vec<String> {
    let (text, separator) = if cfg!(windows) {
        let text = text.replace('/', "\\").to_lowercase();
        let plain = text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned();
        (plain, '\\')
    } else {
        (text.to_owned(), '/')
    };
    text.split(separator)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The profile directories of `build_dir`: those holding `.fingerprint/`,
/// in it or one level below (a target triple's).
fn profiles(build_dir: &Path) -> Vec<PathBuf> {
    let mut profiles = Vec::new();
    for dir in subdirs(build_dir) {
        if dir.join(".fingerprint").is_dir() {
            profiles.push(dir);
        } else {
            profiles.extend(
                subdirs(&dir)
                    .into_iter()
                    .filter(|dir| dir.join(".fingerprint").is_dir()),
            );
        }
    }
    profiles
}

/// The directories in `dir`, never through a link.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// The files in `dir` named `*.d`, never through a link.
fn d_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "d"))
        .collect()
}

/// rustc's dep-info files in `profile`: `deps/`, `examples/`, and each
/// build script's directory in `build/`.
fn dep_info_files(profile: &Path) -> Vec<PathBuf> {
    let mut files = d_files(&profile.join("deps"));
    files.extend(d_files(&profile.join("examples")));
    for script in subdirs(&profile.join("build")) {
        files.extend(d_files(&script));
    }
    files
}

/// The fingerprint directories in `fingerprints`, by name, each with the
/// names of the files in it.
fn fingerprint_dirs(fingerprints: &Path) -> BTreeMap<String, Vec<String>> {
    subdirs(fingerprints)
        .into_iter()
        .filter_map(|dir| {
            let name = dir.file_name()?.to_str()?.to_owned();
            let files = std::fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            Some((name, files))
        })
        .collect()
}

/// The fingerprint directories, among `units`, of the unit whose dep-info
/// file is `<stem>.d`. A crate's name has no `-`, so one in `stem` is
/// where its hash starts.
fn units_of<'a>(stem: &str, units: &'a BTreeMap<String, Vec<String>>) -> Vec<&'a str> {
    match stem.rsplit_once('-') {
        Some((_, hash)) => units
            .keys()
            .filter(|name| {
                name.rsplit_once('-')
                    .is_some_and(|(_, theirs)| theirs == hash)
            })
            .map(String::as_str)
            .collect(),
        None => {
            let target = format!("_{stem}");
            units
                .iter()
                .filter(|(_, files)| {
                    files
                        .iter()
                        .any(|file| file.replace('-', "_").ends_with(&target))
                })
                .map(|(name, _)| name.as_str())
                .collect()
        }
    }
}

/// Whether the rustc dep-info file `dep_info` records an `env-dep` value
/// under one of `prefixes`.
fn names_old_path(dep_info: &Path, prefixes: &[Vec<String>]) -> bool {
    let Ok(bytes) = std::fs::read(dep_info) else {
        return false;
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .filter_map(|line| line.strip_prefix("# env-dep:"))
        .filter_map(|dep| dep.split_once('=').map(|(_, value)| unescape(value)))
        .any(|value| {
            let under = |path: &str| {
                let path = components(path);
                prefixes.iter().any(|prefix| path.starts_with(prefix))
            };
            under(&value)
                || std::env::split_paths(&value).any(|path| under(&path.to_string_lossy()))
        })
}

/// An `env-dep` value as rustc escaped it, unescaped: `\\`, `\n` and `\r`.
fn unescape(value: &str) -> String {
    let mut unescaped = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            unescaped.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => unescaped.push('\n'),
            Some('r') => unescaped.push('\r'),
            Some(other) => unescaped.push(other),
            None => unescaped.push('\\'),
        }
    }
    unescaped
}
