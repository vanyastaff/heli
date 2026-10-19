//! Runtime protection beyond Cargo's compilation locks. A managed run holds
//! the lease of its worktree, shared, for as long as its commands run, also
//! across a daemon crash (`super::runner`); eviction tries the exclusive
//! side of the leases a victim's workspace relates to, and leaves alone the
//! directories of the workspaces a run is in flight in. The namespace is per
//! user, independent of daemon state/configuration, because nested commands
//! can use stores another daemon registered.
//!
//! **Per worktree (#108).** One lease for the whole host froze eviction for
//! as long as any run was in flight, which on a host of busy agents is
//! always. Each worktree has a lease of its own instead,
//! `<activity>/worktrees/<hash>/`, with the worktree's path in `path` so
//! that eviction can tell which workspaces it covers: a workspace inside the
//! worktree, or one around it (a run from a directory outside any
//! repository is in the directory it started in).
//!
//! **What a run's nested cargo built.** A run's tests can start cargo on
//! other workspaces, temporary ones (trybuild, nested builds, with no owner
//! manifest) or other known ones, and read what it built while they run.
//! Each run leaves a marker in its lease while it holds it
//! (`run-<nanos>-<pid>-<n>`), so eviction knows when the oldest run in
//! flight started ([`in_flight_since`]), and leaves alone every directory
//! that changed since then. Older ones, the leftovers of workspaces long
//! gone or idle ones, can go while runs are in flight.
//!
//! An activity namespace that cannot be resolved protects everything, as
//! before ([`available`]).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// An absolute activity namespace for tests/spikes. Every participant that
/// can use the same caches must select the same namespace. Relative paths
/// cannot name one namespace across client and daemon working directories.
pub const DIRECTORY_VAR: &str = "HELI_ACTIVITY_DIR";
const LOCK: &str = ".heli-runtime.lock";

pub fn directory() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(DIRECTORY_VAR).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HELI_ACTIVITY_DIR must be an absolute path",
            ));
        }
        return Ok(path);
    }
    #[cfg(windows)]
    let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(unix)]
    let root = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"));
    root.map(|root| root.join("heli").join("activity"))
        .ok_or_else(|| io::Error::other("cannot locate heli's runtime activity directory"))
}

/// The directory of the leases of worktrees.
const WORKTREES: &str = "worktrees";

/// The file in a worktree's lease directory naming the worktree.
const PATH: &str = "path";

/// The lease root of `worktree` (as [`crate::worktree::of`] gives it), for
/// [`shared`]: created, with the worktree's path recorded, if need be.
pub fn worktree_root(worktree: &Path) -> io::Result<PathBuf> {
    let worktree = super::simplified(worktree);
    let root = directory()?.join(WORKTREES).join(key(&worktree));
    std::fs::create_dir_all(&root)?;
    let path = root.join(PATH);
    if !path.is_file() {
        std::fs::write(&path, worktree.to_string_lossy().as_bytes())?;
    }
    Ok(root)
}

/// A name for the lease of `worktree`: a hash of its path, as places
/// compare (without case on Windows).
fn key(worktree: &Path) -> String {
    use sha2::{Digest, Sha256};
    let spelled = worktree.to_string_lossy();
    let spelled = if cfg!(windows) {
        spelled.to_lowercase()
    } else {
        spelled.into_owned()
    };
    Sha256::digest(spelled.as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether `inner` is `outer` or below it, component by component, without
/// case on Windows.
fn within(inner: &Path, outer: &Path) -> bool {
    let parts = |path: &Path| -> Vec<String> {
        path.components()
            .map(|component| {
                let text = component.as_os_str().to_string_lossy();
                if cfg!(windows) {
                    text.to_lowercase()
                } else {
                    text.into_owned()
                }
            })
            .collect()
    };
    let (inner, outer) = (parts(inner), parts(outer));
    inner.len() >= outer.len() && inner[..outer.len()] == outer[..]
}

/// The lease roots of the worktrees `workspace` relates to, with their
/// paths: those inside it, and those around it.
fn related(workspace: &Path) -> Vec<(PathBuf, PathBuf)> {
    let workspace = super::simplified(workspace);
    let Ok(root) = directory() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root.join(WORKTREES)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let lease = entry.path();
            let worktree = PathBuf::from(std::fs::read_to_string(lease.join(PATH)).ok()?);
            (within(&workspace, &worktree) || within(&worktree, &workspace))
                .then_some((lease, worktree))
        })
        .collect()
}

/// The worktree of a run in flight whose directories `workspace`'s may be,
/// if any; never creates a file. A lease that cannot be opened counts as
/// held.
pub fn busy_for(workspace: &Path) -> Option<PathBuf> {
    related(workspace)
        .into_iter()
        .find_map(|(lease, worktree)| {
            let held = match File::options()
                .read(true)
                .write(true)
                .open(lease.join(LOCK))
            {
                Ok(file) => file.try_lock().is_err(),
                Err(error) => error.kind() != io::ErrorKind::NotFound,
            };
            held.then_some(worktree)
        })
}

/// Takes, without waiting, the exclusive side of every lease `workspace`
/// relates to, for an eviction in it: held until the files drop. The error
/// names the worktree a run is in flight in. A run that starts meanwhile
/// waits for the files to drop before its command starts.
pub fn exclusive_for(workspace: &Path) -> Result<Vec<File>, String> {
    let mut held = Vec::new();
    for (lease, worktree) in related(workspace) {
        let file = match File::options()
            .read(true)
            .write(true)
            .open(lease.join(LOCK))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot open the runtime lease of {}: {error}",
                    worktree.display()
                ));
            }
        };
        file.try_lock()
            .map_err(|_| format!("busy: a run is in flight in {}", worktree.display()))?;
        held.push(file);
    }
    Ok(held)
}

fn open(root: &Path) -> io::Result<File> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(root)?;
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(LOCK))
}

/// A lease held shared, with its run's marker; both go when it drops.
#[derive(Debug)]
pub struct Shared {
    _file: File,
    marker: Option<PathBuf>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(marker) = self.marker.take() {
            let _ = std::fs::remove_file(marker);
        }
    }
}

/// The prefix of a run's marker in a lease.
const MARKER: &str = "run-";

/// Shared by arbitrarily many commands, held through test/exec runtime;
/// it leaves a marker saying since when (see the module docs).
pub fn shared(root: &Path) -> io::Result<Shared> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static MARKERS: AtomicU64 = AtomicU64::new(0);
    let file = open(root)?;
    file.lock_shared()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let marker = root.join(format!(
        "{MARKER}{nanos}-{}-{}",
        std::process::id(),
        MARKERS.fetch_add(1, Ordering::Relaxed)
    ));
    // What holds it, for whoever wonders why a directory is left alone.
    // Without a marker the lease still protects its worktree; only the
    // directories elsewhere lose the run's start.
    // `args` would panic on an argument that is not Unicode.
    let holder = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let marker = std::fs::write(&marker, holder.as_bytes())
        .ok()
        .map(|()| marker);
    Ok(Shared {
        _file: file,
        marker,
    })
}

/// Whether the activity namespace resolves; when it does not, eviction
/// leaves everything alone.
pub fn available() -> bool {
    directory().is_ok()
}

/// When the oldest run in flight started, in seconds since 1970, from the
/// markers in the worktree leases runs hold (shared: an eviction's
/// exclusive hold is no run); `None` with no run in flight. A run that holds
/// its lease but has not left its marker yet started now, and a lease that
/// cannot be opened counts as held since ever. The markers of a lease no run
/// holds are stale (a crash): they are deleted.
pub fn in_flight_since() -> Option<u64> {
    let root = directory().ok()?;
    let entries = std::fs::read_dir(root.join(WORKTREES)).ok()?;
    let mut oldest: Option<u64> = None;
    for entry in entries.flatten() {
        let lease = entry.path();
        let markers: Vec<(PathBuf, u64)> = std::fs::read_dir(&lease)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let nanos: u128 = name.strip_prefix(MARKER)?.split('-').next()?.parse().ok()?;
                Some((entry.path(), u64::try_from(nanos / 1_000_000_000).ok()?))
            })
            .collect();
        let file = match File::options()
            .read(true)
            .write(true)
            .open(lease.join(LOCK))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            // Cannot be told: held since ever.
            Err(_) => {
                oldest = Some(0);
                continue;
            }
        };
        if file.try_lock().is_ok() {
            // No run holds it: what is left was left by a crash.
            for (marker, _) in &markers {
                let _ = std::fs::remove_file(marker);
            }
            continue;
        }
        // Runs hold it shared; an eviction holds it exclusively, for a
        // moment, and is no run.
        if file.try_lock_shared().is_err() {
            continue;
        }
        // A run that has its lease but not yet its marker started now.
        let since = markers
            .iter()
            .map(|(_, since)| *since)
            .min()
            .unwrap_or_else(crate::clock::now);
        oldest = Some(oldest.map_or(since, |oldest| oldest.min(since)));
    }
    oldest
}

#[cfg(test)]
mod tests {
    use super::within;
    use std::path::Path;

    #[test]
    fn within_compares_components() {
        let (a, b) = if cfg!(windows) {
            (r"D:\Work\Repo\crates\x", r"d:\work\repo")
        } else {
            ("/work/repo/crates/x", "/work/repo")
        };
        assert!(within(Path::new(a), Path::new(b)));
        assert!(!within(Path::new(b), Path::new(a)));
        let sibling = if cfg!(windows) {
            r"D:\Work\Repo2"
        } else {
            "/work/repo2"
        };
        assert!(!within(Path::new(sibling), Path::new(b)));
    }
}
