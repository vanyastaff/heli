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
//! - A slot is taken by reading a byte, and given back by writing one.
//! - A run pool is a FIFO like the host pool, beside it, created empty. One
//!   left at its path by an earlier daemon is replaced: whoever still holds
//!   it keeps it, under no name.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;
use rustix::io::Errno;

use super::{Opened, check_name};

/// The byte of a free slot, as cargo's jobserver writes it.
const TOKEN: u8 = b'|';

pub struct Jobserver {
    /// The daemon's own open file description: nonblocking.
    fifo: File,
    path: PathBuf,
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
                check_owned_fifo(&path, &metadata)?;
                is_held(&path)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                make_fifo(&path)?;
                false
            }
            Err(error) => return Err(error),
        };
        let fifo = open_nonblocking(&path)?;
        let opened = if held {
            Opened::Adopted
        } else {
            (&fifo).write_all(&vec![TOKEN; slots as usize])?;
            Opened::Created
        };
        Ok((
            Self {
                fifo,
                path,
                auth,
                slots,
            },
            opened,
        ))
    }

    /// A new, empty FIFO at `path`, whose directory exists, for up to
    /// `maximum` slots. A FIFO of this user's already there is replaced.
    pub fn create_empty(path: &OsStr, maximum: u32) -> io::Result<Self> {
        let auth = format!("fifo:{}", check_name(path)?);
        let path = PathBuf::from(path);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                check_owned_fifo(&path, &metadata)?;
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        make_fifo(&path)?;
        let fifo = open_nonblocking(&path)?;
        Ok(Self {
            fifo,
            path,
            auth,
            slots: maximum,
        })
    }

    /// The FIFO's path, which other pools are named after.
    pub fn name(&self) -> &OsStr {
        self.path.as_os_str()
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

    /// Takes a free slot if there is one, without waiting: the daemon's
    /// file description is nonblocking.
    pub fn try_take(&self) -> io::Result<bool> {
        loop {
            match (&self.fifo).read(&mut [0u8]) {
                Ok(1) => return Ok(true),
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Gives one slot back; `false` when the FIFO is full, and the slot is
    /// dropped.
    pub fn give(&self) -> io::Result<bool> {
        loop {
            match (&self.fifo).write(&[TOKEN]) {
                Ok(1) => return Ok(true),
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// A waiter that takes slots as the processes on this FIFO do: a
    /// blocking read, in line with theirs.
    pub fn waiter(&self) -> io::Result<Waiter> {
        let fifo = OpenOptions::new().read(true).write(true).open(&self.path)?;
        Ok(Waiter { fifo })
    }

    /// Removes the FIFO's path; whoever holds it open keeps it.
    pub fn remove(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Takes slots in line with the other processes reading a FIFO.
pub struct Waiter {
    /// Its own open file description, blocking.
    fifo: File,
}

impl Waiter {
    /// Waits until it takes a slot. A blocking read cannot be cancelled, so
    /// it is `true` even after a [`Waiter::wake`], whose byte it then took.
    pub fn take(&self) -> io::Result<bool> {
        loop {
            match (&self.fifo).read(&mut [0u8]) {
                Ok(1) => return Ok(true),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the FIFO has no writer",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Nothing to do: a blocking read is not cancelled.
    pub fn arm(&self) {}

    /// Nothing to do: a blocking read is not cancelled, so the wait in
    /// progress goes on until it takes a slot.
    pub fn cancel(&self) {}

    /// Ends the wait in progress for good: writes a byte for it to take,
    /// which whoever takes it must drop.
    pub fn wake(&self) {
        let _ = (&self.fifo).write_all(&[TOKEN]);
    }
}

/// Refuses `path` unless it is a FIFO of this user.
fn check_owned_fifo(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_fifo() && metadata.uid() == rustix::process::geteuid().as_raw() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a FIFO of this user", path.display()),
        ))
    }
}

/// Makes a FIFO at `path`, with mode exactly 0600, whatever the umask.
fn make_fifo(path: &Path) -> io::Result<()> {
    mkfifo(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Opens the FIFO at `path` for reading and writing, nonblocking: an open
/// file description of the daemon's own, so the processes that open the
/// FIFO by path keep blocking reads.
fn open_nonblocking(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(OFlags::NONBLOCK.bits() as i32)
        .open(path)
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
