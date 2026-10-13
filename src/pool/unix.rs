//! The host pool on Unix: a FIFO holding one byte per free slot.
//!
//! - It has mode 0600, in a directory heli creates with mode 0700, and must
//!   belong to the user.
//! - The daemon holds it open for reading and writing, nonblocking: its own
//!   open file description, so the cargos that open the FIFO by path keep
//!   blocking reads. `FIONREAD` counts the free slots.
//! - A FIFO's bytes live in the kernel only while some process holds it
//!   open. Before opening it, the daemon asks whether anyone does: opening
//!   it write-only and nonblocking fails with `ENXIO` when nobody has it
//!   open for reading, and every jobserver client opens it for reading and
//!   writing. Held, it is adopted as it is; not held, it is empty, and the
//!   daemon writes N bytes into it.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;
use rustix::io::Errno;

use super::{Opened, check_name};

/// The byte of a free slot, as cargo's jobserver writes it.
const TOKEN: u8 = b'|';

pub struct Jobserver {
    fifo: File,
    auth: String,
    slots: u32,
}

impl Jobserver {
    pub fn open(path: &OsStr, slots: u32) -> io::Result<(Self, Opened)> {
        let auth = format!("fifo:{}", check_name(path)?);
        let path = PathBuf::from(path);
        if let Some(dir) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let held = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_fifo()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a FIFO of this user", path.display()),
                    ));
                }
                is_held(&path)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                mkfifo(&path)?;
                // Exactly 0600, whatever the umask.
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                false
            }
            Err(error) => return Err(error),
        };
        let fifo = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlags::NONBLOCK.bits() as i32)
            .open(&path)?;
        let opened = if held {
            Opened::Adopted
        } else {
            (&fifo).write_all(&vec![TOKEN; slots as usize])?;
            Opened::Created
        };
        Ok((Self { fifo, auth, slots }, opened))
    }

    /// What `--jobserver-auth=` names: `fifo:<path>`.
    pub fn auth(&self) -> &str {
        &self.auth
    }

    /// N, as the pool was opened with: a FIFO has no maximum of its own.
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// The free slots: the bytes in the FIFO.
    pub fn free(&self) -> io::Result<u32> {
        let bytes = rustix::io::ioctl_fionread(&self.fifo)?;
        Ok(u32::try_from(bytes).unwrap_or(u32::MAX))
    }
}

/// Whether some process has the FIFO at `path` open for reading.
fn is_held(path: &Path) -> io::Result<bool> {
    match OpenOptions::new()
        .write(true)
        .custom_flags(OFlags::NONBLOCK.bits() as i32)
        .open(path)
    {
        Ok(_probe) => Ok(true),
        Err(error) if error.raw_os_error() == Some(Errno::NXIO.raw_os_error()) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(not(target_vendor = "apple"))]
fn mkfifo(path: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, Mode};
    Ok(rustix::fs::mkfifoat(CWD, path, Mode::RUSR | Mode::WUSR)?)
}

#[cfg(target_vendor = "apple")]
fn mkfifo(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the FIFO's path"))?;
    // SAFETY: `path` is NUL-terminated and outlives the call.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
