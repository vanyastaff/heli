//! Named pipes that only the user can open.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe,
    SetNamedPipeHandleState, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Each pipe instance's buffer, in each direction.
const BUFFER: u32 = 64 * 1024;

/// The longest single wait for a free instance of a busy pipe.
const BUSY_POLL: Duration = Duration::from_millis(50);

/// The daemon's pipe: always one instance waiting for the next client.
pub struct Listener {
    name: Vec<u16>,
    security: OwnerOnly,
    pending: File,
}

impl Listener {
    /// Creates the pipe `path` (`\\.\pipe\…`); fails if it already exists.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let name = wide(path.as_os_str());
        let security = OwnerOnly::new()?;
        let pending = create_instance(&name, &security, true)?;
        Ok(Self {
            name,
            security,
            pending,
        })
    }

    /// Creates the next instances of the pipe `path`, which a retiring
    /// daemon of this user created first (docs/daemon.md, "Handover").
    pub fn join(path: &Path) -> io::Result<Self> {
        let name = wide(path.as_os_str());
        let security = OwnerOnly::new()?;
        let pending = create_instance(&name, &security, false)?;
        Ok(Self {
            name,
            security,
            pending,
        })
    }

    /// Waits for the next client.
    pub fn accept(&mut self) -> io::Result<Stream> {
        loop {
            // SAFETY: `pending` owns a pipe instance; the call is synchronous.
            let connected = unsafe { ConnectNamedPipe(self.pending.as_raw_handle(), null_mut()) };
            // SAFETY: reads this thread's last error.
            let error = unsafe { GetLastError() };
            // A fresh instance waits before this one is handed over, so a
            // client rarely finds every instance busy.
            let next = create_instance(&self.name, &self.security, false)?;
            let instance = std::mem::replace(&mut self.pending, next);
            if connected != 0 || error == ERROR_PIPE_CONNECTED {
                return Stream::new(instance);
            }
            // The client left before it was accepted: wait for the next one.
        }
    }
}

/// One connection. Dropping it closes it; the other side still reads what
/// was written before (only `DisconnectNamedPipe`, which heli never calls,
/// would drop it).
pub struct Stream(File, Option<Instant>);

impl Stream {
    fn new(file: File) -> io::Result<Self> {
        let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
        // SAFETY: an open duplex pipe handle, and a pointer to its new mode.
        if unsafe { SetNamedPipeHandleState(file.as_raw_handle(), &mode, null(), null()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file, None))
    }

    pub fn set_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        self.1 = deadline;
        Ok(())
    }
    /// Whether the other side still has the connection open; asks without
    /// reading or blocking.
    pub fn connected(&self) -> bool {
        let mut available = 0;
        // SAFETY: a pipe handle; a peek of no bytes only counts them. It
        // fails (ERROR_BROKEN_PIPE) once the other side has closed.
        let peeked = unsafe {
            PeekNamedPipe(
                self.0.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        };
        peeked != 0
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if buf.is_empty() {
                return Ok(0);
            }
            let mut available = 0;
            // SAFETY: an open pipe and one writable count. The standard
            // library maps an empty nonblocking pipe to EOF, so inspect
            // availability before reading, without consuming any bytes.
            if unsafe {
                PeekNamedPipe(
                    self.0.as_raw_handle(),
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if available == 0 {
                match self.1 {
                    Some(deadline) => super::retry_until(deadline)?,
                    None => std::thread::sleep(Duration::from_millis(1)),
                }
                continue;
            }
            let count = buf.len().min(available as usize);
            return self.0.read(&mut buf[..count]);
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A nonblocking byte-pipe write larger than its buffer can make no
        // progress even when empty (e.g. a long selected log line). Split
        // it before retrying; write_all preserves each successful offset.
        let buf = &buf[..buf.len().min(BUFFER as usize)];
        loop {
            match self.0.write(buf) {
                Ok(0) if !buf.is_empty() => match self.1 {
                    Some(deadline) => super::retry_until(deadline)?,
                    None => std::thread::sleep(Duration::from_millis(1)),
                },
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Connects to the daemon's pipe `path`, if it belongs to this user.
///
/// Every instance of the pipe taken (`ERROR_PIPE_BUSY`) means a daemon
/// holds it: one between two accepts, or slowed down by a loaded host. So
/// it waits for a free instance, and tries again, until `deadline`; only
/// then does it fail with that error. When the pipe goes meanwhile (its
/// daemon has exited), it fails as when there is none.
pub fn connect(path: &Path, deadline: Instant) -> io::Result<Stream> {
    let name = wide(path.as_os_str());
    loop {
        // SAFETY: `name` is NUL-terminated. Identification level: the
        // server may learn who connects but cannot act as the client.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: a fresh handle the file now owns.
            let pipe = unsafe { File::from_raw_handle(handle) };
            check_owner(&pipe)?;
            return Stream::new(pipe);
        }
        let error = io::Error::last_os_error();
        let now = Instant::now();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) || now >= deadline {
            return Err(error);
        }
        // In short waits: one ends at once when the pipe has gone, but
        // might not notice it go while it waits.
        let wait = (deadline - now).min(BUSY_POLL).as_millis().max(1);
        // SAFETY: `name` is NUL-terminated.
        unsafe {
            WaitNamedPipeW(name.as_ptr(), u32::try_from(wait).unwrap_or(u32::MAX));
        }
    }
}

/// A security descriptor owned by the current user, whose DACL allows that
/// user alone.
struct OwnerOnly {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl OwnerOnly {
    fn new() -> io::Result<Self> {
        let user = sid_string(token_information(TokenUser)?.user())?;
        // The owner is the user's SID, elevated or not: by default an
        // elevated process gives what it creates to the Administrators
        // group, which clients do not trust. Protected DACL (no inherited
        // entries), one entry: full access for the user.
        let sddl = wide(OsStr::new(&format!("O:{user}D:P(A;;GA;;;{user})")));
        let mut descriptor = null_mut();
        // SAFETY: `sddl` is NUL-terminated; the descriptor is freed on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

/// A new instance of the pipe `name`, not inheritable, owner-only, local
/// clients only.
fn create_instance(name: &[u16], security: &OwnerOnly, first: bool) -> io::Result<File> {
    let attributes = security.attributes();
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: `name` is NUL-terminated and `attributes` outlives the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle the file now owns.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Fails unless the pipe's owner is this user's SID, which only this user
/// (or a process with the privilege to take ownership) can give a pipe.
/// Not the Administrators group: any elevated process of any
/// administrator can create a pipe it owns.
fn check_owner(pipe: &File) -> io::Result<()> {
    let mut owner: PSID = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the handle is open; `owner` points into `descriptor`, freed below.
    let status = unsafe {
        GetSecurityInfo(
            pipe.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let trusted = (|| -> io::Result<bool> {
        let user = token_information(TokenUser)?;
        // SAFETY: both are valid SIDs.
        Ok(unsafe { EqualSid(owner, user.user()) != 0 })
    })();
    // SAFETY: allocated by GetSecurityInfo.
    unsafe {
        LocalFree(descriptor);
    }
    if trusted? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the endpoint belongs to another user",
        ))
    }
}

/// A token information buffer, aligned for the structures in it.
struct TokenInformation(Vec<u64>);

impl TokenInformation {
    /// The SID of a `TokenUser` buffer.
    fn user(&self) -> PSID {
        // SAFETY: filled by GetTokenInformation(TokenUser), aligned.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// `class` of this process's token.
fn token_information(class: TOKEN_INFORMATION_CLASS) -> io::Result<TokenInformation> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo-handle of this process; `token` is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut needed = 0;
    // SAFETY: a size query, with no buffer.
    unsafe {
        GetTokenInformation(token, class, null_mut(), 0, &mut needed);
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` holds at least `needed` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: opened above.
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(error);
    }
    Ok(TokenInformation(buffer))
}

/// The SID of the user this process runs as, `S-1-5-…`: the one user the
/// endpoint admits, and the one the logon task runs as (`crate::autostart`).
pub fn user_sid() -> io::Result<String> {
    sid_string(token_information(TokenUser)?.user())
}

/// `sid` as `S-1-5-…`.
fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    // SAFETY: `sid` is valid; the string is freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a NUL-terminated string from ConvertSidToStringSidW.
    let string = unsafe {
        let length = (0..).take_while(|&index| *text.add(index) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, length))
    };
    // SAFETY: allocated by ConvertSidToStringSidW.
    unsafe {
        LocalFree(text.cast());
    }
    Ok(string)
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}
