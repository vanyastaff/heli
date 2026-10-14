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
//! - **Its path outlives its daemon**, and once nobody holds it, it is empty:
//!   a bare cargo given its path would build one job at a time, without a
//!   warning. So once the daemon is gone, its warden
//!   ([`remove_when_released`]) removes the FIFO as soon as no process holds
//!   it; a bare cargo then cannot open it, warns, and builds on its own.
//! - **The pool's lock**, `<fifo>.lock` beside it, keeps the two apart: a
//!   daemon holds it for its whole life, from before it opens the FIFO, and
//!   a warden only while it looks, so it never removes a FIFO that a daemon
//!   owns.
//! - Whether another process holds it open, later: a descriptor of it among
//!   the user's processes (Linux: `/proc/<pid>/fd`; macOS: `proc_pidinfo`);
//!   elsewhere unknown.
//! - The daemon takes slots by reading bytes on its own nonblocking
//!   description, and gives them by writing; a FIFO has no maximum count.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::OFlags;
use rustix::io::Errno;

use super::{Opened, check_name};

/// The byte of a free slot, as cargo's jobserver writes it.
const TOKEN: u8 = b'|';

/// How long a daemon tries to take the pool's lock: a warden holds it for a
/// moment, another daemon for good.
const OWNER_PATIENCE: Duration = Duration::from_secs(1);

/// How often a warden looks again at a FIFO that some process still holds.
const WARDEN_POLL: Duration = Duration::from_millis(250);

pub struct Jobserver {
    fifo: File,
    /// The pool's lock, held while the daemon lives.
    _owner: File,
    auth: String,
    slots: u32,
}

impl Jobserver {
    pub fn open(path: &OsStr, slots: u32) -> io::Result<(Self, Opened)> {
        let auth = auth(path)?;
        let path = PathBuf::from(path);
        if let Some(dir) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let owner = own(&path)?;
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
        let jobserver = Self {
            fifo,
            _owner: owner,
            auth,
            slots,
        };
        Ok((jobserver, opened))
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

    /// Takes up to `slots` free slots, never waiting (the daemon's file
    /// description is nonblocking); returns how many.
    pub fn take(&self, slots: u32) -> io::Result<u32> {
        let mut buffer = vec![0u8; slots as usize];
        let mut taken = 0;
        while taken < buffer.len() {
            match (&self.fifo).read(&mut buffer[taken..]) {
                Ok(0) => break,
                Ok(read) => taken += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(u32::try_from(taken).unwrap_or(slots))
    }

    /// Gives `slots` slots to the pool; a FIFO has no maximum.
    pub fn give(&self, slots: u32) -> io::Result<u32> {
        (&self.fifo).write_all(&vec![TOKEN; slots as usize])?;
        Ok(slots)
    }

    /// Whether a process other than this one has the FIFO open: one of the
    /// user's processes has a descriptor of it under `/proc/<pid>/fd`.
    #[cfg(target_os = "linux")]
    pub fn held_by_others(&self) -> io::Result<bool> {
        let fifo = self.fifo.metadata()?;
        let me = std::process::id();
        let user = rustix::process::geteuid().as_raw();
        for process in fs::read_dir("/proc")?.flatten() {
            let Some(pid) = process
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == me || !process.metadata().is_ok_and(|owner| owner.uid() == user) {
                continue;
            }
            // A process that is gone, or not readable, holds nothing of ours.
            let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
                continue;
            };
            for descriptor in descriptors.flatten() {
                if fs::metadata(descriptor.path())
                    .is_ok_and(|file| file.dev() == fifo.dev() && file.ino() == fifo.ino())
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether a process other than this one has the FIFO open: one of the
    /// processes this user may inspect has a descriptor of it
    /// (`proc_pidinfo`, `proc_pidfdinfo`).
    #[cfg(target_os = "macos")]
    pub fn held_by_others(&self) -> io::Result<bool> {
        use std::ffi::c_int;

        /// `struct proc_fileinfo` (sys/proc_info.h).
        #[repr(C)]
        struct ProcFileInfo {
            open_flags: u32,
            status: u32,
            offset: i64,
            kind: i32,
            guard_flags: u32,
        }
        /// `struct vnode_fdinfo` (sys/proc_info.h).
        #[repr(C)]
        struct VnodeFdInfo {
            file: ProcFileInfo,
            vnode: libc::vnode_info,
        }
        const PROC_PIDFDVNODEINFO: c_int = 1;
        const DESCRIPTOR: usize = size_of::<libc::proc_fdinfo>();

        let fifo = self.fifo.metadata()?;
        let me = std::process::id();
        for pid in crate::processes::pids()? {
            if u32::try_from(pid).is_ok_and(|pid| pid == me) {
                continue;
            }
            // SAFETY: no buffer: asks for the size of the list.
            let size = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)
            };
            // A process that is gone, or not ours to see, holds nothing.
            let Ok(size) = usize::try_from(size) else {
                continue;
            };
            // Room for descriptors opened meanwhile.
            let mut list: Vec<libc::proc_fdinfo> = (0..size / DESCRIPTOR + 16)
                .map(|_| libc::proc_fdinfo {
                    proc_fd: 0,
                    proc_fdtype: 0,
                })
                .collect();
            let bytes = c_int::try_from(list.len() * DESCRIPTOR).unwrap_or(c_int::MAX);
            // SAFETY: a buffer of the size given.
            let filled = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    list.as_mut_ptr().cast(),
                    bytes,
                )
            };
            let Ok(filled) = usize::try_from(filled) else {
                continue;
            };
            list.truncate(filled / DESCRIPTOR);
            for descriptor in list
                .iter()
                .filter(|descriptor| descriptor.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
            {
                // SAFETY: plain old data, all zeros until filled.
                let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
                let wanted = size_of::<VnodeFdInfo>() as c_int;
                // SAFETY: a buffer of the size given.
                let filled = unsafe {
                    libc::proc_pidfdinfo(
                        pid,
                        descriptor.proc_fd,
                        PROC_PIDFDVNODEINFO,
                        (&raw mut info).cast(),
                        wanted,
                    )
                };
                // A descriptor closed meanwhile has no information; one of
                // another size means this layout is not the system's, and
                // who holds the pool is not known.
                if filled > 0 && filled != wanted {
                    return Err(io::Error::other(format!(
                        "proc_pidfdinfo gave {filled} bytes for a vnode, not {wanted}"
                    )));
                }
                if filled == wanted
                    && info.vnode.vi_stat.vst_dev == fifo.dev() as u32
                    && info.vnode.vi_stat.vst_ino == fifo.ino()
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether a process other than this one has the FIFO open: not known
    /// here.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn held_by_others(&self) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot tell who holds a FIFO here",
        ))
    }
}

/// What `--jobserver-auth=` names for the FIFO at `path`: `fifo:<path>`.
pub fn auth(path: &OsStr) -> io::Result<String> {
    Ok(format!("fifo:{}", check_name(path)?))
}

/// Takes the lock of the pool at `path` for a daemon.
fn own(path: &Path) -> io::Result<File> {
    let lock = open_lock(path, true)?;
    let deadline = Instant::now() + OWNER_PATIENCE;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("another heli daemon owns the pool {}", path.display()),
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
}

/// Opens the lock of the pool at `path`, `<path>.lock`; with `create`,
/// creates it if need be.
fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    OpenOptions::new()
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .open(lock)
}

/// The warden's work, once the daemon that owned the FIFO at `path` is gone:
/// removes the FIFO as soon as no process holds it, looking again every
/// [`WARDEN_POLL`] while one does. Returns without removing it once a daemon
/// owns the pool again, or when the FIFO is gone or not this user's.
pub fn remove_when_released(path: &Path) {
    loop {
        // The daemon made the lock. Without it there is nothing to remove,
        // and a warden never creates files.
        let Ok(lock) = open_lock(path, false) else {
            return;
        };
        if lock.try_lock().is_err() {
            return;
        }
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_fifo()
                    && metadata.uid() == rustix::process::geteuid().as_raw() => {}
            _ => return,
        }
        match is_held(path) {
            Ok(true) => {}
            Ok(false) => {
                let _ = fs::remove_file(path);
                return;
            }
            Err(_) => return,
        }
        drop(lock);
        std::thread::sleep(WARDEN_POLL);
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
