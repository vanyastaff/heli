//! Volumes: cargo hardlinks final artifacts out of a build directory on the
//! same volume and copies them across volumes, so a worktree's build
//! directory belongs in the store on its own volume.
//!
//! A volume is known by its root, the directory it is mounted at: on Windows
//! what `GetVolumePathNameW` says (`D:\`, or a folder a volume is mounted
//! in); elsewhere the highest directory above a path on the same device.
//! Paths that do not exist yet belong to the volume of their nearest
//! existing ancestor.

use std::path::{Path, PathBuf};

/// The root of the volume that holds `path`, or `None` when no part of it
/// exists.
pub fn root(path: &Path) -> Option<PathBuf> {
    let path = std::path::absolute(path).ok()?;
    let existing = path.ancestors().find(|dir| dir.exists())?;
    platform_root(existing)
}

/// Whether `a` and `b` are on the same volume, when both can be told.
pub fn same(a: &Path, b: &Path) -> bool {
    match (root(a), root(b)) {
        (Some(a), Some(b)) => spelled(&a) == spelled(&b),
        _ => false,
    }
}

/// `root` as volume roots compare: on Windows without case.
fn spelled(root: &Path) -> String {
    let root = root.to_string_lossy();
    if cfg!(windows) {
        root.to_lowercase()
    } else {
        root.into_owned()
    }
}

#[cfg(windows)]
fn platform_root(existing: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

    let wide: Vec<u16> = existing
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buffer = vec![0u16; 32_768];
    let length = u32::try_from(buffer.len()).expect("a small buffer");
    // SAFETY: `wide` ends with a NUL, and `buffer` holds `length` units.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), buffer.as_mut_ptr(), length) };
    if ok == 0 {
        return None;
    }
    let end = buffer.iter().position(|&unit| unit == 0)?;
    Some(PathBuf::from(OsString::from_wide(&buffer[..end])))
}

#[cfg(unix)]
fn platform_root(existing: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let device = std::fs::metadata(existing).ok()?.dev();
    let mut root = existing;
    for dir in existing.ancestors().skip(1) {
        match std::fs::metadata(dir) {
            Ok(metadata) if metadata.dev() == device => root = dir,
            _ => break,
        }
    }
    Some(root.to_path_buf())
}
