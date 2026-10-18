//! The daemon's owner, in debug builds (the test seam).
//!
//! An integration test starts a daemon of its own, which it kills as it
//! ends. A test that is killed itself (a test runner's timeout, a cancelled
//! run) never gets there, and its daemon is detached from it (`crate::launch`):
//! it would go on running with nobody to stop it, its pool and its runs with
//! it. So in debug builds, [`VAR`] names an owner, by its process id: once that
//! process is gone, the daemon kills its runs in flight and exits. Release
//! builds ignore it: a user's daemon outlives the client that started it.

/// In debug builds, the process id of the process whose end ends the daemon.
pub const VAR: &str = "HELI_TEST_OWNER";

/// The owner [`VAR`] names, in a debug build.
pub fn pid() -> Option<u32> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(VAR).ok()?.parse().ok()
}

/// Waits until process `pid` is gone; returns at once when it is already.
#[cfg(windows)]
pub fn wait_for_exit(pid: u32) {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: opens a handle, owned below; null when the process is gone.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return;
    }
    // SAFETY: a fresh handle.
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    // SAFETY: an open process handle with SYNCHRONIZE access.
    unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
}

/// Waits until process `pid` is gone; returns at once when it is already.
#[cfg(unix)]
pub fn wait_for_exit(pid: u32) {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    while rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
