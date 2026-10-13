//! A stand-in for the daemon at a fixture's endpoint, for what a real daemon
//! of this build cannot produce: a daemon that speaks another protocol
//! version, one that hangs up or never answers, or something that is not
//! heli at all.
//!
//! It speaks only the handshake every version of heli shares: `HELI` and the
//! protocol version as a little-endian u32, first from the client, then from
//! the daemon.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::thread::JoinHandle;

/// How the fake answers the client's hello.
#[derive(Debug, Clone, Copy)]
pub enum Answer {
    /// `HELI` and this protocol version.
    Version(u32),
    /// These bytes.
    Bytes([u8; 8]),
    /// Nothing: it hangs up.
    HangUp,
    /// Nothing: it keeps the connection open until the client closes it.
    Never,
}

/// A fake daemon serving one client.
pub struct FakeDaemon {
    served: JoinHandle<Vec<u8>>,
}

impl FakeDaemon {
    /// Listens at `endpoint` and serves one client: reads its hello and
    /// answers as told.
    pub fn start(endpoint: &OsStr, answer: Answer) -> Self {
        let accept = listen(endpoint);
        let served = std::thread::spawn(move || {
            let mut stream = accept();
            let mut hello = [0u8; 8];
            if stream.read_exact(&mut hello).is_err() {
                return Vec::new();
            }
            assert_eq!(&hello[..4], b"HELI", "the client's hello");
            let bytes = match answer {
                Answer::Version(version) => {
                    let mut bytes = *b"HELI\0\0\0\0";
                    bytes[4..].copy_from_slice(&version.to_le_bytes());
                    bytes
                }
                Answer::Bytes(bytes) => bytes,
                Answer::HangUp => return Vec::new(),
                Answer::Never => {
                    let _ = stream.read_to_end(&mut Vec::new());
                    return Vec::new();
                }
            };
            if stream.write_all(&bytes).is_err() {
                return Vec::new();
            }
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
            rest
        });
        Self { served }
    }

    /// Waits until the client has hung up, and returns what it sent after
    /// the handshake.
    pub fn received(self) -> Vec<u8> {
        self.served.join().expect("the fake daemon panicked")
    }
}

/// Starts listening at `endpoint` now (so a client cannot miss it), and
/// returns how to accept the one client.
#[cfg(unix)]
fn listen(endpoint: &OsStr) -> impl FnOnce() -> std::os::unix::net::UnixStream + Send + 'static {
    let listener =
        std::os::unix::net::UnixListener::bind(endpoint).expect("bind the fake daemon's socket");
    move || listener.accept().expect("accept a client").0
}

#[cfg(windows)]
fn listen(endpoint: &OsStr) -> impl FnOnce() -> std::fs::File + Send + 'static {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;

    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    let name: Vec<u16> = endpoint.encode_wide().chain([0]).collect();
    // SAFETY: `name` is NUL-terminated; default security, no inheritance.
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            65_536,
            65_536,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(pipe, INVALID_HANDLE_VALUE, "create the fake daemon's pipe");
    // SAFETY: a fresh pipe handle this file now owns.
    let file = unsafe { std::fs::File::from_raw_handle(pipe) };
    move || {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the handle is a pipe instance owned by `file`.
        let connected = unsafe { ConnectNamedPipe(file.as_raw_handle(), std::ptr::null_mut()) };
        // SAFETY: reads this thread's last error.
        let error = unsafe { GetLastError() };
        assert!(
            connected != 0 || error == ERROR_PIPE_CONNECTED,
            "accept a client: error {error}"
        );
        file
    }
}
