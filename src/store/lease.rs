//! Runtime protection beyond Cargo's compilation locks. All managed runs
//! share this guard; GC only tries its exclusive side and skips busy runs.
//! The namespace is per user, independent of daemon state/configuration,
//! because nested commands can use stores another daemon registered.

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

/// Shared by arbitrarily many commands, held through test/exec runtime.
pub fn shared(root: &Path) -> io::Result<File> {
    let file = open(root)?;
    file.lock_shared()?;
    Ok(file)
}

/// GC never queues behind a command. Keep this handle through the rename;
/// neither trash deletion nor surveying needs to delay a new command.
pub fn exclusive() -> Result<File, String> {
    let file = open(&directory().map_err(|error| error.to_string())?)
        .map_err(|error| format!("cannot open runtime activity guard: {error}"))?;
    file.try_lock()
        .map_err(|error| format!("busy: managed command runtime activity: {error}"))?;
    Ok(file)
}

/// Read-only planning must not create even a lock file or directory.
pub fn busy() -> bool {
    let Ok(root) = directory() else { return true };
    let file = match File::options().read(true).write(true).open(root.join(LOCK)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    file.try_lock().is_err()
}
