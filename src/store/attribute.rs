//! Whose a build directory is when no heli run built in it (#88): bare cargo,
//! and nested cargo in tests (trybuild, generated projects), whose temporary
//! workspaces each get a build directory of their own under a user-wide
//! `build.build-dir`.
//!
//! Cargo names such a directory `<2 hex>/<14 hex>`, the hash of its
//! workspace's manifest path (`{workspace-path-hash}`: cargo's
//! `short_hash` of the canonical `Cargo.toml` path, `StableSipHasher128`
//! over the path's `Hash`, as little-endian hex). The hash cannot be
//! inverted, but a candidate can be checked: the compiler records its
//! working directory, the workspace root, in the object files of the crates
//! it compiles incrementally, the workspace's own. heli reads the paths in
//! the smallest of them and takes the candidate whose hash is the
//! directory's name, and nothing on a guess. A build that wrote no object
//! file (`cargo check`, `clippy`) leaves no path, and its directory stays
//! unknown.
//!
//! A directory attributed gets an owner manifest (`super::owner`), so that
//! it is attributed once: then it is live or an orphan like any other. One
//! that cannot be gets `.heli/unattributed`, and is tried again only once
//! cargo has built in it since.

use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use super::owner::{self, Owner};

/// The object files read for paths, smallest first.
const OBJECTS: usize = 4;

/// At most this much of an object file is read.
const READ: u64 = 8 << 20;

/// Cargo's `{workspace-path-hash}` of the manifest `manifest`, as spelled:
/// the two path components it names under the store.
pub fn workspace_path_hash(manifest: &Path) -> (String, String) {
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    manifest.hash(&mut hasher);
    let hex: String = Hasher::finish(&hasher)
        .to_le_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (hex[..2].to_owned(), hex[2..].to_owned())
}

/// The workspace root whose manifest hashes to `build_dir`'s name, among
/// the paths its object files hold.
pub fn infer(build_dir: &Path) -> Option<PathBuf> {
    let inner = build_dir.file_name()?.to_str()?.to_ascii_lowercase();
    let outer = build_dir
        .parent()?
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    let mut tried = std::collections::HashSet::new();
    for path in object_paths(build_dir) {
        for dir in path.ancestors() {
            if !tried.insert(dir.to_path_buf()) {
                continue;
            }
            let manifest = dir.join("Cargo.toml");
            if spellings(&manifest)
                .iter()
                .any(|spelled| workspace_path_hash(spelled) == (outer.clone(), inner.clone()))
            {
                return Some(dir.to_path_buf());
            }
        }
    }
    None
}

/// Attributes `build_dir`, which has no owner manifest, at `now`: writes the
/// manifest of the workspace [`infer`] finds, and returns it. Marks a directory it cannot attribute, and
/// tries one marked again only when cargo has written in it since.
pub fn attribute(build_dir: &Path, now: u64) -> Option<Owner> {
    let marker = build_dir.join(".heli").join("unattributed");
    if let Ok(marked) = std::fs::metadata(&marker).and_then(|meta| meta.modified())
        && newest_build(build_dir).is_none_or(|newest| newest <= marked)
    {
        return None;
    }
    let Some(workspace) = infer(build_dir) else {
        let _ = std::fs::create_dir_all(build_dir.join(".heli"))
            .and_then(|()| std::fs::write(&marker, ""));
        return None;
    };
    let owner = Owner {
        workspace,
        // A heli run in the workspace records it.
        repository: None,
        created_at: now,
        last_run: 0,
    };
    owner::write(build_dir, &owner).ok()?;
    let _ = std::fs::remove_file(&marker);
    Some(owner)
}

/// When cargo last wrote in a profile directory of `build_dir`: the newest
/// of its profiles' `.fingerprint/` directories.
fn newest_build(build_dir: &Path) -> Option<std::time::SystemTime> {
    profiles(build_dir)
        .into_iter()
        .filter_map(|profile| {
            std::fs::metadata(profile.join(".fingerprint"))
                .and_then(|meta| meta.modified())
                .ok()
        })
        .max()
}

/// The profile directories of `build_dir`: `<dir>/<profile>`, and
/// `<dir>/<target triple>/<profile>`.
fn profiles(build_dir: &Path) -> Vec<PathBuf> {
    let mut profiles = Vec::new();
    for dir in super::subdirs(build_dir) {
        if dir.join(".fingerprint").is_dir() {
            profiles.push(dir);
        } else {
            profiles.extend(
                super::subdirs(&dir)
                    .into_iter()
                    .filter(|dir| dir.join(".fingerprint").is_dir()),
            );
        }
    }
    profiles
}

/// The absolute paths in the smallest object files of `build_dir`'s
/// incremental sessions.
fn object_paths(build_dir: &Path) -> Vec<PathBuf> {
    let mut objects = Vec::new();
    for profile in profiles(build_dir) {
        for unit in super::subdirs(&profile.join("incremental")) {
            for session in super::subdirs(&unit) {
                let Ok(entries) = std::fs::read_dir(&session) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "o")
                        && let Ok(meta) = entry.metadata()
                    {
                        objects.push((meta.len(), path));
                    }
                }
            }
        }
    }
    objects.sort();
    let mut paths = Vec::new();
    for (_, object) in objects.into_iter().take(OBJECTS) {
        let Ok(file) = std::fs::File::open(&object) else {
            continue;
        };
        let mut bytes = Vec::new();
        if std::io::Read::read_to_end(&mut std::io::Read::take(file, READ), &mut bytes).is_err() {
            continue;
        }
        for path in absolute_paths(&bytes) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// The runs of printable bytes in `bytes` that read as absolute paths.
fn absolute_paths(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|byte| !(0x20..0x7f).contains(byte))
        .filter(|run| run.len() >= 3)
        .filter_map(|run| std::str::from_utf8(run).ok())
        .filter_map(|text| {
            // A path may follow other text in the run: start at its root.
            let start = text
                .find(":\\")
                .map(|at| at.saturating_sub(1))
                .or_else(|| (!cfg!(windows)).then(|| text.find('/')).flatten())?;
            let path = PathBuf::from(&text[start..]);
            path.is_absolute().then_some(path)
        })
        .collect()
}

/// The spellings of `manifest` cargo may have hashed: the canonical path
/// when it exists. Otherwise (the workspace is gone) the path as spelled,
/// the canonical path of its nearest existing ancestor followed by the rest,
/// and on Windows the verbatim (`\\?\`) form that canonicalizing gives.
fn spellings(manifest: &Path) -> Vec<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(manifest) {
        return vec![canonical];
    }
    let mut spellings = vec![manifest.to_path_buf()];
    let mut rest = Vec::new();
    let mut base = manifest;
    while let Some(parent) = base.parent() {
        rest.push(base.file_name().map(PathBuf::from).unwrap_or_default());
        if let Ok(canonical) = std::fs::canonicalize(parent) {
            let mut path = canonical;
            for part in rest.iter().rev() {
                path.push(part);
            }
            spellings.push(path);
            break;
        }
        base = parent;
    }
    if cfg!(windows)
        && let Some(Component::Prefix(prefix)) = manifest.components().next()
        && !prefix.kind().is_verbatim()
    {
        let mut verbatim = std::ffi::OsString::from(r"\\?\");
        verbatim.push(manifest.as_os_str());
        spellings.push(PathBuf::from(verbatim));
    }
    spellings.dedup();
    spellings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash is cargo's: the build directory `cargo metadata` names for
    /// this crate under a templated `build-dir` ends in it.
    #[test]
    fn hashes_as_cargo_does() {
        let dir = tempfile::tempdir().expect("dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).expect("dir");
        std::fs::write(
            ws.join("Cargo.toml"),
            "[package]\nname = \"ws\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write");
        std::fs::write(ws.join("src").join("lib.rs"), "").expect("write");
        let store = dir.path().join("store");
        let output =
            std::process::Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()))
                .args([
                    "metadata",
                    "--format-version",
                    "1",
                    "--no-deps",
                    "--offline",
                ])
                .current_dir(&ws)
                .env(
                    "CARGO_BUILD_BUILD_DIR",
                    format!("{}/{{workspace-path-hash}}", store.display()),
                )
                .output()
                .expect("run cargo metadata");
        assert!(output.status.success(), "{output:?}");
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).expect("metadata");
        let build_dir = PathBuf::from(metadata["build_directory"].as_str().expect("a build dir"));
        let inner = build_dir
            .file_name()
            .expect("inner")
            .to_string_lossy()
            .into_owned();
        let outer = build_dir
            .parent()
            .and_then(Path::file_name)
            .expect("outer")
            .to_string_lossy()
            .into_owned();
        let manifest = std::fs::canonicalize(ws.join("Cargo.toml")).expect("canonical");
        assert_eq!(workspace_path_hash(&manifest), (outer, inner));
    }

    #[test]
    fn finds_absolute_paths_in_bytes() {
        let root = if cfg!(windows) {
            r"C:\work\ws"
        } else {
            "/work/ws"
        };
        let mut bytes = b"\x00\x01junk".to_vec();
        bytes.extend_from_slice(format!("x{root}\x00").as_bytes());
        bytes.extend_from_slice(b"\x02src\\lib.rs\x00");
        assert_eq!(absolute_paths(&bytes), [PathBuf::from(root)]);
    }

    /// A gone workspace is found in the directory cargo built it in, from
    /// a path the object files hold.
    #[test]
    fn infers_a_gone_workspace_from_its_objects() {
        let dir = tempfile::tempdir().expect("dir");
        let ws = dir.path().join("gone");
        // One of the spellings cargo may have hashed.
        let manifest = spellings(&ws.join("Cargo.toml")).pop().expect("a spelling");
        let (outer, inner) = workspace_path_hash(&manifest);
        let build = dir.path().join("store").join(outer).join(inner);
        let session = build
            .join("debug")
            .join("incremental")
            .join("ws-1")
            .join("s-1");
        std::fs::create_dir_all(&session).expect("dir");
        std::fs::create_dir_all(build.join("debug").join(".fingerprint")).expect("dir");
        let mut object = b"\x00\x00".to_vec();
        object.extend_from_slice(ws.display().to_string().as_bytes());
        object.push(0);
        std::fs::write(session.join("a.o"), object).expect("write");
        assert_eq!(infer(&build), Some(ws.clone()));

        let owner = attribute(&build, 7).expect("attributed");
        assert_eq!(owner.workspace, ws);
        assert_eq!(owner.repository, None);
        assert_eq!(owner::read(&build), Some(owner));
    }
}
