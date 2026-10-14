//! The transport between `cargo heli` and its daemon: a local endpoint that
//! only the user who runs the daemon can open, and the byte streams of its
//! connections. What travels over them is `crate::protocol`'s business.
//!
//! - **Windows**: a named pipe, owned by the user's SID (set explicitly:
//!   elevated, a process would give it to the Administrators group). Its
//!   DACL grants access to the user's SID alone (no Everyone, no
//!   Administrators, no SYSTEM entry); it rejects remote clients; and the
//!   daemon creates it with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so it never
//!   joins a pipe someone else created first. A client connects at
//!   identification level only (a server cannot impersonate it) and talks
//!   only to a pipe owned by its own user's SID, elevated or not: never to
//!   one owned by the Administrators group, which any administrator's
//!   elevated process can create.
//! - **Unix**: a Unix socket with mode 0600, in a directory heli creates
//!   with mode 0700. A client talks only to a socket owned by its own uid.
//!
//! Built on the standard library and `windows-sys` (already heli's only
//! platform dependency) rather than an IPC crate: the owner-only security
//! descriptor and the owner check need the raw Win32 calls anyway, and the
//! connections are plain blocking streams.

use std::io;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Listener, Stream, connect};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Listener, Stream, connect, user_sid};

/// True when `error` from [`connect`] means no daemon is listening (as
/// opposed to one that is there but cannot be used).
pub fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// True when `error` from [`connect`] means a daemon holds the endpoint but
/// had no connection free for this client by the deadline (on Windows,
/// every instance of its pipe stayed taken): it is there, not yet
/// reachable.
pub fn is_busy(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
        error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
    }
    #[cfg(unix)]
    {
        let _ = error;
        false
    }
}
