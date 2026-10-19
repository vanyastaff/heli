//! Whether anything compiles for cargo now: half of the quiescence that the
//! host pool's top-up waits for (docs/host-pool.md, "Bounded top-up").
//!
//! The processes that run cargo's jobs, and hold its slots, are `rustc`,
//! `rustdoc`, `clippy-driver` and build scripts (`build-script-*`); and
//! `cargo` itself, which takes a slot before it starts the job it is for, so
//! may hold one while none of those runs (#26). Among the processes that
//! can open the pool:
//!
//! - **Windows**: those of this logon session, where the pool's semaphore
//!   lives (`WTSEnumerateProcessesW`: names and sessions in one call).
//! - **Linux**: the user's (the owner of `/proc/<pid>`), by
//!   `/proc/<pid>/comm`, which the kernel cuts to 15 bytes
//!   (`build-script-bu`).
//! - **macOS**: every process, by the file name of `proc_pidpath`.

use std::io;

/// Whether a process that runs a cargo job is alive.
pub fn compiling() -> io::Result<bool> {
    platform::compiling()
}

/// Every process, by id (macOS).
#[cfg(target_os = "macos")]
pub use platform::pids;

/// Whether a process named `name` (its file name, without `.exe`) runs a
/// cargo job, or is a cargo, which may hold a slot for one it has not
/// started yet.
fn runs_a_job(name: &str) -> bool {
    matches!(name, "rustc" | "rustdoc" | "clippy-driver" | "cargo")
        || name.starts_with("build-script-")
}

#[cfg(windows)]
mod platform {
    use std::io;

    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW, WTSFreeMemory,
    };

    use super::runs_a_job;

    pub fn compiling() -> io::Result<bool> {
        let session = crate::locations::session_id();
        let mut processes: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
        let mut count = 0;
        // SAFETY: the local server, version 1; the list is freed below.
        let ok = unsafe {
            WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut processes, &mut count)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: WTSEnumerateProcessesW returned `count` entries there.
        let list = unsafe { std::slice::from_raw_parts(processes, count as usize) };
        let found = list.iter().any(|process| {
            // SAFETY: a name in the list, which lives until it is freed.
            process.SessionId == session && runs_a_job(&unsafe { name(process.pProcessName) })
        });
        // SAFETY: the list WTSEnumerateProcessesW allocated, freed once.
        unsafe { WTSFreeMemory(processes.cast()) };
        Ok(found)
    }

    /// The file name at `wide`, a NUL-terminated string or null, in
    /// lowercase and without `.exe`.
    unsafe fn name(wide: *const u16) -> String {
        if wide.is_null() {
            return String::new();
        }
        // SAFETY: the caller's: NUL-terminated.
        let units = unsafe {
            let length = (0..).take_while(|&index| *wide.add(index) != 0).count();
            std::slice::from_raw_parts(wide, length)
        };
        let name = String::from_utf16_lossy(units).to_ascii_lowercase();
        match name.strip_suffix(".exe") {
            Some(stem) => stem.to_owned(),
            None => name,
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;
    use std::os::unix::fs::MetadataExt;

    use super::runs_a_job;

    pub fn compiling() -> io::Result<bool> {
        let user = rustix::process::geteuid().as_raw();
        for process in std::fs::read_dir("/proc")?.flatten() {
            let name = process.file_name();
            if !name
                .to_str()
                .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
                || !process.metadata().is_ok_and(|owner| owner.uid() == user)
            {
                continue;
            }
            // A process that is gone has no name.
            if let Ok(comm) = std::fs::read_to_string(process.path().join("comm"))
                && runs_a_job(comm.trim_end_matches('\n'))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::OsStr;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::runs_a_job;

    /// Every process, by id.
    pub fn pids() -> io::Result<Vec<libc::pid_t>> {
        // SAFETY: no buffer: asks for the number of processes.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        // Room for processes started meanwhile.
        let mut pids: Vec<libc::pid_t> = vec![0; count as usize + 64];
        let bytes = i32::try_from(pids.len() * size_of::<libc::pid_t>()).unwrap_or(i32::MAX);
        // SAFETY: a buffer of the size given.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        pids.truncate(count as usize);
        Ok(pids)
    }

    pub fn compiling() -> io::Result<bool> {
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        for pid in pids()? {
            // SAFETY: a buffer of the size given.
            let length =
                unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
            // A process that is gone, or not ours to see, has no path.
            let Ok(length) = usize::try_from(length) else {
                continue;
            };
            let file = Path::new(OsStr::from_bytes(&path[..length.min(path.len())])).file_name();
            if file.and_then(OsStr::to_str).is_some_and(runs_a_job) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use std::io;

    pub fn compiling() -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "heli cannot list this system's processes",
        ))
    }
}
