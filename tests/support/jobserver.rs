//! A fixture's host pool seen from a process that uses it, as cargo's
//! jobserver client does: it opens the pool by name, takes slots and gives
//! them back.
//!
//! Dropping a [`Client`] closes it: slots it took and did not give back are
//! lost to the pool, as when a cargo is killed while holding them.

use std::ffi::OsStr;

/// An open hold on a pool.
pub struct Client {
    #[cfg(windows)]
    semaphore: std::os::windows::io::OwnedHandle,
    #[cfg(unix)]
    fifo: std::fs::File,
}

impl Client {
    /// Opens the pool `name` (a semaphore name, or a FIFO's path) as
    /// cargo's jobserver client does, taking no slot.
    #[cfg(windows)]
    pub fn open(name: &OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;

        use windows_sys::Win32::System::Threading::{
            OpenSemaphoreW, SEMAPHORE_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
        };

        let wide: Vec<u16> = name.encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated; the handle is owned below.
        let handle = unsafe {
            OpenSemaphoreW(
                SYNCHRONIZATION_SYNCHRONIZE | SEMAPHORE_MODIFY_STATE,
                0,
                wide.as_ptr(),
            )
        };
        assert!(!handle.is_null(), "open the semaphore");
        Self {
            // SAFETY: a semaphore handle just opened, this client's from here.
            semaphore: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
        }
    }

    /// Opens the pool `name` (a semaphore name, or a FIFO's path) as
    /// cargo's jobserver client does, taking no slot.
    #[cfg(unix)]
    pub fn open(name: &OsStr) -> Self {
        let fifo = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .expect("open the FIFO");
        Self { fifo }
    }

    /// Takes one free slot; there must be one.
    #[cfg(windows)]
    pub fn take(&self) {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        // SAFETY: a semaphore handle with SYNCHRONIZE; a zero timeout.
        let taken = unsafe { WaitForSingleObject(self.semaphore.as_raw_handle(), 0) };
        assert_eq!(taken, WAIT_OBJECT_0, "take a slot");
    }

    /// Takes one free slot; there must be one.
    #[cfg(unix)]
    pub fn take(&self) {
        use std::io::Read;
        (&self.fifo).read_exact(&mut [0u8]).expect("take a slot");
    }

    /// Gives `slots` slots to the pool, taken or not.
    #[cfg(windows)]
    pub fn give(&self, slots: i32) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: a semaphore handle with SEMAPHORE_MODIFY_STATE.
        let released = unsafe {
            windows_sys::Win32::System::Threading::ReleaseSemaphore(
                self.semaphore.as_raw_handle(),
                slots,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(released, 0, "give {slots} slots");
    }

    /// Gives `slots` slots to the pool, taken or not.
    #[cfg(unix)]
    pub fn give(&self, slots: i32) {
        use std::io::Write;
        let bytes = vec![b'|'; usize::try_from(slots).expect("a slot count")];
        (&self.fifo).write_all(&bytes).expect("give slots");
    }
}
