//! Provenance for a store created as an exclusively regenerable cache.
//! Existing stores are never adopted, even when empty. Without this proof,
//! an unknown workspace retains the conservative eviction policy.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

const MARKER: &str = ".heli-cache-root.json";

#[derive(Serialize, Deserialize)]
struct Root {
    schema: u32,
    root: std::path::PathBuf,
}

/// Creates a new explicitly configured store and records its cache ownership.
/// An existing directory is left untouched, including an incomplete marker.
pub fn prepare_new(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("a cache root must be absolute"));
    }
    let parent = path.parent().ok_or_else(|| io::Error::other("no parent"))?;
    fs::create_dir_all(parent)?;
    // Resolve ordinary platform aliases (notably macOS /var -> /private/var)
    // before creating anything. The marker is bound to this canonical root.
    let parent = fs::canonicalize(parent)?;
    let path = parent.join(
        path.file_name()
            .ok_or_else(|| io::Error::other("no name"))?,
    );
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    }
    let marker = Root {
        schema: 1,
        root: fs::canonicalize(&path)?,
    };
    let bytes = serde_json::to_vec(&marker).map_err(io::Error::other)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join(MARKER))?;
    file.write_all(&bytes)?;
    file.sync_all()
}

/// Whether this exact root still has a valid, regular-file ownership marker.
pub fn owns(path: &Path) -> bool {
    if !path.is_absolute() || !real_directory(path) {
        return false;
    }
    let marker = path.join(MARKER);
    let Ok(metadata) = fs::symlink_metadata(&marker) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        return false;
    }
    let Ok(bytes) = fs::read(marker) else {
        return false;
    };
    let Ok(marker) = serde_json::from_slice::<Root>(&bytes) else {
        return false;
    };
    marker.schema == 1 && fs::canonicalize(path).ok().as_ref() == Some(&marker.root)
}

fn real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// A Cargo-shaped unknown directory inside an owned root may use tier 5.
/// An unfamiliar lock layout is never authorized by age alone.
pub fn unknown_is_reclaimable(dir: &super::Dir) -> bool {
    let Some(store) = &dir.store else {
        return false;
    };
    if !owns(store) {
        return false;
    }
    let Ok(relative) = dir.path.strip_prefix(store) else {
        return false;
    };
    if relative.components().count() != 2 {
        return false;
    }
    let mut reached = store.clone();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return false;
        }
        reached.push(component);
        if !real_directory(&reached) {
            return false;
        }
    }
    let profiles = super::lock_dirs(&dir.path);
    !super::unknown_layout(&profiles)
        && profiles.iter().any(|profile| {
            super::LOCKS.iter().any(|name| {
                fs::symlink_metadata(profile.join(name)).is_ok_and(|metadata| metadata.is_file())
            })
        })
}
