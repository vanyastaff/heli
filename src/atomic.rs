//! Replacing a file whole, so that a crash, a kill or a full volume never
//! leaves it half-written: `setup` rewrites the user's shell profiles and
//! editor settings, `autostart` writes service definitions and the record
//! clients start the daemon by.
//!
//! The content goes to a temporary file beside the target, renamed over it
//! once complete. What the target had stays: its permissions (a hook that
//! was executable stays executable, whatever the umask), and its place when
//! it is a link (a profile kept as a link into a dotfiles repository is
//! written there, and the link stays). A target that refuses a plain write
//! (read-only) refuses this one too: the rename alone would not ask.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replaces the file at `path` with `contents`, creating its directory.
pub fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = target(path)?;
    // Opened for writing first, as a plain write would open it: a read-only
    // target fails here. What it has is kept for the replacement.
    let permissions = match std::fs::OpenOptions::new().write(true).open(&target) {
        Ok(file) => Some(file.metadata()?.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    // A name of this write's own, so that two writers never share a file.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let temporary = target.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temporary, contents)
        .and_then(|()| match permissions {
            Some(permissions) => std::fs::set_permissions(&temporary, permissions),
            None => Ok(()),
        })
        .and_then(|()| std::fs::rename(&temporary, &target))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
}

/// Where the content goes: `path`, or what it links to, through every link
/// (a dangling one is followed too, where a plain write would make the
/// file); the directory of a file that does not exist is created.
fn target(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => match std::fs::canonicalize(path) {
            Ok(real) => Ok(real),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let linked = std::fs::read_link(path)?;
                let linked = match path.parent() {
                    Some(dir) if linked.is_relative() => dir.join(linked),
                    _ => linked,
                };
                target(&linked)
            }
            Err(error) => Err(error),
        },
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            Ok(path.to_path_buf())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::write;

    /// The file's directory is made, and nothing is left beside the file.
    #[test]
    fn a_new_file_gets_its_directory_and_no_neighbour() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a/b/file");
        write(&path, b"one").expect("write");
        write(&path, b"two").expect("write again");
        assert_eq!(std::fs::read(&path).expect("read"), b"two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .expect("list")
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["file"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_link_stays_and_the_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let real = dir.path().join("dotfiles/profile");
        std::fs::create_dir_all(real.parent().unwrap()).expect("dir");
        std::fs::write(&real, "old").expect("write");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let link = dir.path().join("profile");
        std::os::unix::fs::symlink("dotfiles/profile", &link).expect("link");
        write(&link, b"new").expect("write through the link");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read(&real).expect("read"), b"new");
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // Dangling: the file is made where the link points.
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink("made/later", &dangling).expect("link");
        write(&dangling, b"made").expect("write through a dangling link");
        assert_eq!(
            std::fs::read(dir.path().join("made/later")).expect("read"),
            b"made"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            // Nothing is read-only to root.
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings");
        std::fs::write(&path, "kept").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("mode");
        let error = write(&path, b"new").expect_err("refused");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&path).expect("read"), b"kept");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings"]);
    }
}
