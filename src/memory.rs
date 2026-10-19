//! The machine's memory, as the memory governor watches it (docs/host-pool.md,
//! "Memory governor"): available physical memory and, on Windows, available
//! commit, where allocations fail at the commit limit before RAM is full.
//!
//! - **Windows**: `GlobalMemoryStatusEx` for physical memory (available is
//!   the standby, free and zero lists); `K32GetPerformanceInfo` for commit,
//!   `CommitLimit − CommitTotal` system-wide (`ullAvailPageFile` would
//!   shrink to the limit of a job the daemon runs in).
//! - **Linux**: `MemTotal` and `MemAvailable` from `/proc/meminfo`. Under the
//!   default overcommit policy allocations do not fail at a commit limit, so
//!   there is no commit figure.
//! - **macOS**: `hw.memsize`, and `host_statistics64`: what is neither
//!   anonymous (less purgeable), wired nor compressed is available, as
//!   Activity Monitor counts it.

use std::io;

/// One reading, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub total: u64,
    pub available: u64,
    /// Windows only.
    pub commit_available: Option<u64>,
}

/// Reads the machine's memory now.
pub fn sample() -> io::Result<Sample> {
    platform::sample()
}

#[cfg(windows)]
mod platform {
    use std::io;

    use windows_sys::Win32::System::ProcessStatus::{
        K32GetPerformanceInfo, PERFORMANCE_INFORMATION,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    use super::Sample;

    pub fn sample() -> io::Result<Sample> {
        let mut status = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: a structure of the length it states.
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut performance = PERFORMANCE_INFORMATION::default();
        // SAFETY: a structure of the size given.
        let ok = unsafe {
            K32GetPerformanceInfo(
                &mut performance,
                size_of::<PERFORMANCE_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let pages = performance
            .CommitLimit
            .saturating_sub(performance.CommitTotal);
        Ok(Sample {
            total: status.ullTotalPhys,
            available: status.ullAvailPhys,
            commit_available: Some((pages as u64).saturating_mul(performance.PageSize as u64)),
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;

    use super::Sample;

    pub fn sample() -> io::Result<Sample> {
        let text = std::fs::read_to_string("/proc/meminfo")?;
        // `<name>:   <n> kB`
        let field = |name: &str| {
            text.lines()
                .find_map(|line| {
                    let value = line.strip_prefix(name)?.strip_prefix(':')?;
                    value.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
                })
                .map(|kb| kb.saturating_mul(1024))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("no {name} in /proc/meminfo"),
                    )
                })
        };
        Ok(Sample {
            total: field("MemTotal")?,
            available: field("MemAvailable")?,
            commit_available: None,
        })
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::sync::OnceLock;

    use super::Sample;

    pub fn sample() -> io::Result<Sample> {
        let mut total: u64 = 0;
        let mut size = size_of::<u64>();
        // SAFETY: a u64 buffer of the size given, for a u64 value.
        let failed = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut total).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0;
        if failed {
            return Err(io::Error::last_os_error());
        }
        // Each call adds a reference to the port: take it once.
        static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
        // SAFETY: no arguments; returns this host's port.
        #[allow(deprecated, reason = "libc's own, as sysinfo uses it")]
        let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
        // SAFETY: plain old data, all zeros until filled.
        let mut stat: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: a vm_statistics64 buffer, with its size in integers.
        let result = unsafe {
            libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                (&raw mut stat).cast(),
                &mut count,
            )
        };
        if result != libc::KERN_SUCCESS {
            return Err(io::Error::other(format!(
                "host_statistics64 failed: {result}"
            )));
        }
        // SAFETY: no arguments.
        let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .map_err(|_| io::Error::other("no page size"))?;
        let anonymous =
            u64::from(stat.internal_page_count).saturating_sub(u64::from(stat.purgeable_count));
        let used = anonymous
            .saturating_add(u64::from(stat.wire_count))
            .saturating_add(u64::from(stat.compressor_page_count))
            .saturating_mul(page);
        Ok(Sample {
            total,
            available: total.saturating_sub(used),
            commit_available: None,
        })
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use std::io;

    use super::Sample;

    pub fn sample() -> io::Result<Sample> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "heli cannot read this system's memory",
        ))
    }
}
