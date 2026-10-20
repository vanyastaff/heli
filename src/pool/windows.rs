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
//! - Whether another process holds it: the object's handle count
//!   (`NtQueryObject`, `ObjectBasicInformation`) is above the daemon's one.
//! - A release past the maximum count is all or nothing, and fails with
//!   `ERROR_TOO_MANY_POSTS`.
//! - A run pool is a semaphore like the host pool, created with no free
//!   slot. A name that already exists belongs to some other process, and is
//!   refused rather than adopted.

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS, ERROR_TOO_MANY_POSTS,
    GetLastError, HANDLE, NTSTATUS, SetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateSemaphoreW, GetCurrentProcess, INFINITE, ReleaseSemaphore, ResetEvent,
    SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

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

/// `PUBLIC_OBJECT_BASIC_INFORMATION` (winternl.h).
#[repr(C)]
#[derive(Default)]
struct PublicObjectBasicInformation {
    attributes: u32,
    granted_access: u32,
    handle_count: u32,
    pointer_count: u32,
    reserved: [u32; 10],
}

/// `ObjectBasicInformation`, the information class.
const OBJECT_BASIC_INFORMATION: i32 = 0;

windows_link::link!("ntdll.dll" "system" fn NtQueryObject(
    handle: HANDLE,
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
        let (semaphore, name, opened) = create(name, slots, slots)?;
        // An adopted semaphore keeps the maximum it was created with.
        let maximum = query(&semaphore)?.maximum_count;
        let jobserver = Self {
            semaphore,
            name,
            slots: u32::try_from(maximum).unwrap_or(0),
        };
        Ok((jobserver, opened))
    }

    /// A semaphore has no owner to release: a new daemon adopts it.
    pub fn disown(&self) -> io::Result<()> {
        Ok(())
    }

    /// A new semaphore `name` with no free slot and room for `maximum`. A
    /// semaphore of that name that some process holds is refused.
    pub fn create_empty(name: &OsStr, maximum: u32) -> io::Result<Self> {
        let (semaphore, name, opened) = create(name, 0, maximum)?;
        if opened == Opened::Adopted {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("the semaphore {name} already exists"),
            ));
        }
        Ok(Self {
            semaphore,
            name,
            slots: maximum,
        })
    }

    /// The semaphore's name, which other pools are named after.
    pub fn name(&self) -> &OsStr {
        OsStr::new(&self.name)
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

    /// Takes up to `slots` free slots, never waiting; returns how many.
    pub fn take(&self, slots: u32) -> io::Result<u32> {
        let mut taken = 0;
        while taken < slots {
            // SAFETY: a semaphore handle with every access; a zero timeout
            // never waits.
            match unsafe { WaitForSingleObject(self.semaphore.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => taken += 1,
                WAIT_TIMEOUT => break,
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Ok(taken)
    }

    /// Gives `slots` slots to the pool, as many as fit below its maximum;
    /// returns how many.
    pub fn give(&self, slots: u32) -> io::Result<u32> {
        // A release past the maximum changes nothing and fails, so one at a
        // time: as many as fit.
        let mut given = 0;
        while given < slots && self.release_one()? {
            given += 1;
        }
        Ok(given)
    }

    /// Whether a process other than this one has the semaphore open: it has
    /// more handles than the daemon's one.
    pub fn held_by_others(&self) -> io::Result<bool> {
        let mut information = PublicObjectBasicInformation::default();
        // SAFETY: a handle, and a buffer of the size given.
        let status = unsafe {
            NtQueryObject(
                self.semaphore.as_raw_handle(),
                OBJECT_BASIC_INFORMATION,
                (&raw mut information).cast(),
                size_of::<PublicObjectBasicInformation>() as u32,
                std::ptr::null_mut(),
            )
        };
        if status < 0 {
            return Err(io::Error::other(format!(
                "NtQueryObject failed: status {:#010x}",
                status as u32
            )));
        }
        Ok(information.handle_count > 1)
    }

    /// A waiter that takes slots as the processes on this semaphore do:
    /// in line with them.
    pub fn waiter(&self) -> io::Result<Waiter> {
        let mut duplicate: HANDLE = std::ptr::null_mut();
        // SAFETY: duplicates a handle this process owns, within this
        // process, not inheritable.
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.semaphore.as_raw_handle(),
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle, owned from here on.
        let semaphore = unsafe { OwnedHandle::from_raw_handle(duplicate) };
        // SAFETY: an unnamed manual-reset event, not signalled, not
        // inheritable.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Waiter {
            semaphore,
            // SAFETY: a fresh handle, owned from here on.
            cancel: unsafe { OwnedHandle::from_raw_handle(event) },
        })
    }

    /// Nothing to remove: a semaphore goes with its last handle.
    pub fn remove(&self) {}

    /// Releases one slot; `false` when the semaphore is at its maximum.
    fn release_one(&self) -> io::Result<bool> {
        // SAFETY: a semaphore handle with every access; the previous count
        // is not asked for.
        let released =
            unsafe { ReleaseSemaphore(self.semaphore.as_raw_handle(), 1, std::ptr::null_mut()) };
        if released != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_TOO_MANY_POSTS as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

/// Creates the semaphore `name` with `free` of `maximum` slots free, or
/// opens the one that exists (then ignoring the counts).
fn create(name: &OsStr, free: u32, maximum: u32) -> io::Result<(OwnedHandle, String, Opened)> {
    let name = auth(name)?;
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let free = i32::try_from(free).map_err(io::Error::other)?;
    let maximum = i32::try_from(maximum).map_err(io::Error::other)?;
    // SAFETY: `wide` is NUL-terminated; no security attributes, so the
    // handle is not inherited. The last error tells a new object from an
    // existing one, so it is cleared first.
    let (handle, error) = unsafe {
        SetLastError(0);
        let handle = CreateSemaphoreW(std::ptr::null(), free, maximum, wide.as_ptr());
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
    Ok((semaphore, name, opened))
}

/// Takes slots in line with the other processes waiting on a semaphore,
/// until it is cancelled.
pub struct Waiter {
    semaphore: OwnedHandle,
    /// Signalled to cancel a wait.
    cancel: OwnedHandle,
}

impl Waiter {
    /// Waits until it takes a slot (`true`) or is cancelled (`false`).
    pub fn take(&self) -> io::Result<bool> {
        let handles = [self.semaphore.as_raw_handle(), self.cancel.as_raw_handle()];
        // SAFETY: two valid handles; returns when either is signalled. When
        // both are, the semaphore, the first, wins.
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } {
            WAIT_OBJECT_0 => Ok(true),
            event if event == WAIT_OBJECT_0 + 1 => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Lets the next [`Waiter::take`] wait: called before it, and never
    /// while another thread may be cancelling.
    pub fn arm(&self) {
        // SAFETY: an event handle this waiter owns.
        unsafe { ResetEvent(self.cancel.as_raw_handle()) };
    }

    /// Ends the wait in progress, or the next, without a slot.
    pub fn cancel(&self) {
        // SAFETY: an event handle this waiter owns.
        unsafe { SetEvent(self.cancel.as_raw_handle()) };
    }

    /// Ends the wait in progress for good: the same as a cancel here.
    pub fn wake(&self) {
        self.cancel();
    }
}

/// What `--jobserver-auth=` names for the semaphore `name`: the name, which
/// must be ASCII.
pub fn auth(name: &OsStr) -> io::Result<String> {
    let name = check_name(name)?;
    if !name.is_ascii() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} cannot name a semaphore cargo opens: it must be ASCII"),
        ));
    }
    Ok(name.to_owned())
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
