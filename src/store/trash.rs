//! The trash: where an eviction puts its victim before anything is deleted.
//!
//! Under cargo's lock, a victim is only renamed into the trash of its
//! directory, which is on its volume, so the rename takes constant time:
//! `<store>/.trash/` for a build directory's, `<target>/.heli-trash/` for a
//! registered `target/` directory's. What is in the trash is deleted later,
//! with no lock held, at low I/O priority ([`lower_io_priority`]): a cargo
//! that starts meanwhile finds its directory without the victim, and is not
//! held up. A trash that is a link is never used, and deleting never goes
//! through a link: a link in the trash is removed, not followed.
//!
//! Neither trash is a build directory (`<store>/<2 hex digits>/<hex
//! digits>`), so nothing in it is ever listed, counted or recycled.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::registry::Registry;
use super::{Dir, Kind};
use crate::config::StoreConfig;

/// A store's trash, in the store.
pub const STORE_TRASH: &str = ".trash";

/// A registered `target/` directory's trash, in it.
pub const TARGET_TRASH: &str = ".heli-trash";

/// The trash of `dir`.
pub fn of(dir: &Dir) -> Option<PathBuf> {
    match dir.kind {
        Kind::Build => dir.store.as_ref().map(|store| store.join(STORE_TRASH)),
        Kind::Target => Some(dir.path.join(TARGET_TRASH)),
        Kind::Scratch => Some(super::scratch::trash()),
    }
}

/// Every trash there may be: each store's, each registered `target/`
/// directory's, and the scratch copies' (`super::scratch::trash`).
pub fn all(config: &StoreConfig, registry: &Registry) -> Vec<PathBuf> {
    let stores = super::stores(config, registry)
        .into_iter()
        .map(|store| store.join(STORE_TRASH));
    let targets = registry
        .targets
        .iter()
        .map(|target| target.path.join(TARGET_TRASH));
    stores
        .chain(targets)
        .chain(std::iter::once(super::scratch::trash()))
        .collect()
}

/// Renames `victim` into `trash`, created if need be, under a name of its
/// own; returns where it is now.
pub fn put(victim: &Path, trash: &Path) -> io::Result<PathBuf> {
    static PUTS: AtomicU64 = AtomicU64::new(0);
    let name = victim
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let place = trash.join(format!(
        "{nanos}-{}-{}-{name}",
        std::process::id(),
        PUTS.fetch_add(1, Ordering::Relaxed)
    ));
    // Twice: a trash emptied at the same time may go away in between.
    let mut tries = 0;
    loop {
        tries += 1;
        match std::fs::symlink_metadata(trash) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "{} is not a directory of its own",
                    trash.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match std::fs::create_dir(trash) {
                    Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                        return Err(error);
                    }
                    _ => {}
                }
            }
            Err(error) => return Err(error),
        }
        match std::fs::rename(victim, &place) {
            Ok(()) => return Ok(place),
            Err(error)
                if tries < 2 && error.kind() == io::ErrorKind::NotFound && victim.exists() => {}
            Err(error) => return Err(error),
        }
    }
}

/// The bytes in `trash`, still to be deleted.
pub fn pending(trash: &Path) -> u64 {
    match std::fs::symlink_metadata(trash) {
        Ok(metadata) if metadata.is_dir() => super::ladder::measure(trash).bytes,
        _ => 0,
    }
}

/// Deletes what is in `trash`, and the trash itself once empty. A link is
/// removed, never followed. What cannot be deleted now (another process is
/// deleting it, or holds a file open in it) is left for later; the first
/// such error is returned after everything else is deleted.
pub fn empty(trash: &Path) -> io::Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(trash) else {
        return Ok(());
    };
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut failed = None;
    for entry in std::fs::read_dir(trash)?.filter_map(Result::ok) {
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(kind) if kind.is_symlink() => remove_link(&path),
            // `remove_dir_all` removes links inside, never what they name.
            Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        match removed {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                failed.get_or_insert(error);
            }
            _ => {}
        }
    }
    // Fails while something is left, or a pass is putting a victim there.
    let _ = std::fs::remove_dir(trash);
    failed.map_or(Ok(()), Err)
}

/// Removes the link at `path`: a symlink to a file or a directory, or a
/// junction.
fn remove_link(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
}

/// Lowers the I/O priority of the calling thread, for deleting the trash:
/// background mode on Windows, the I/O throttle on macOS, and on Linux the
/// lowest nice value, from which the I/O scheduler derives the thread's I/O
/// priority. Best effort: where it cannot, nothing changes.
pub fn lower_io_priority() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        // SAFETY: the pseudo handle of the calling thread is always valid.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
    }
    #[cfg(target_vendor = "apple")]
    {
        const IOPOL_TYPE_DISK: libc::c_int = 0;
        const IOPOL_SCOPE_THREAD: libc::c_int = 1;
        const IOPOL_THROTTLE: libc::c_int = 3;
        unsafe extern "C" {
            fn setiopolicy_np(
                iotype: libc::c_int,
                scope: libc::c_int,
                policy: libc::c_int,
            ) -> libc::c_int;
        }
        // SAFETY: plain values; the call only changes this thread's policy.
        unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE) };
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // On Linux the nice value is the calling thread's own.
        let _ = rustix::process::setpriority_process(None, 19);
    }
}
