//! The host pool on Windows: a named semaphore whose count is the free
//! slots.
//!
//! - `CreateSemaphoreW` creates it, or opens the one that exists under the
//!   name (then ignoring the counts it is given): that is adoption. The
//!   object lives while any handle to it is open, so a semaphore that still
//!   exists is held by some process.
//! - The name has no namespace prefix, so it lives in the logon session's
//!   namespace. The default security descriptor comes from the daemon's
//!   token: the user, SYSTEM and Administrators.
//! - The count is read with `NtQuerySemaphore` (ntdll, not in windows-sys),
//!   which needs `SEMAPHORE_QUERY_STATE`: the handle `CreateSemaphoreW`
//!   returns has every access right.
//! - Cargo's jobserver opens the semaphore with `OpenSemaphoreA`, so the
//!   name must be ASCII.

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HANDLE, NTSTATUS, SetLastError,
};
use windows_sys::Win32::System::Threading::CreateSemaphoreW;

use super::{Opened, check_name};

/// `SEMAPHORE_BASIC_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct SemaphoreBasicInformation {
    current_count: i32,
    maximum_count: i32,
}

/// `SemaphoreBasicInformation`, the information class.
const SEMAPHORE_BASIC_INFORMATION: i32 = 0;

windows_link::link!("ntdll.dll" "system" fn NtQuerySemaphore(
    semaphore: HANDLE,
    information_class: i32,
    information: *mut c_void,
    information_length: u32,
    return_length: *mut u32
) -> NTSTATUS);

pub struct Jobserver {
    semaphore: OwnedHandle,
    name: String,
    slots: u32,
}

impl Jobserver {
    pub fn open(name: &OsStr, slots: u32) -> io::Result<(Self, Opened)> {
        let name = check_name(name)?;
        if !name.is_ascii() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} cannot name a semaphore cargo opens: it must be ASCII"),
            ));
        }
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let count = i32::try_from(slots).map_err(io::Error::other)?;
        // SAFETY: `wide` is NUL-terminated; no security attributes, so the
        // handle is not inherited. The last error tells a new object from an
        // existing one, so it is cleared first.
        let (handle, error) = unsafe {
            SetLastError(0);
            let handle = CreateSemaphoreW(std::ptr::null(), count, count, wide.as_ptr());
            (handle, GetLastError())
        };
        if handle.is_null() {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        // SAFETY: a fresh handle, owned from here on.
        let semaphore = unsafe { OwnedHandle::from_raw_handle(handle) };
        let opened = if error == ERROR_ALREADY_EXISTS {
            Opened::Adopted
        } else {
            Opened::Created
        };
        // An adopted semaphore keeps the maximum it was created with.
        let maximum = query(&semaphore)?.maximum_count;
        let jobserver = Self {
            semaphore,
            name: name.to_owned(),
            slots: u32::try_from(maximum).unwrap_or(0),
        };
        Ok((jobserver, opened))
    }

    /// What `--jobserver-auth=` names: the semaphore.
    pub fn auth(&self) -> &str {
        &self.name
    }

    /// N: the semaphore's maximum count.
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// The free slots: the semaphore's count.
    pub fn free(&self) -> io::Result<u32> {
        let count = query(&self.semaphore)?.current_count;
        Ok(u32::try_from(count).unwrap_or(0))
    }
}

/// The counts of `semaphore`, a handle with `SEMAPHORE_QUERY_STATE`.
fn query(semaphore: &OwnedHandle) -> io::Result<SemaphoreBasicInformation> {
    let mut information = SemaphoreBasicInformation::default();
    // SAFETY: a semaphore handle with query access, and a buffer of the size
    // given.
    let status = unsafe {
        NtQuerySemaphore(
            semaphore.as_raw_handle(),
            SEMAPHORE_BASIC_INFORMATION,
            (&raw mut information).cast(),
            size_of::<SemaphoreBasicInformation>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "NtQuerySemaphore failed: status {:#010x}",
            status as u32
        )));
    }
    Ok(information)
}
