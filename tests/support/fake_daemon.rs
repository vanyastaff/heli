//! A stand-in for the daemon at a fixture's endpoint, for what a real daemon
//! of this build cannot produce: a daemon that speaks another protocol
//! version, one that hangs up or never answers, one that is older and does
//! not know a request, or something that is not heli at all.
//!
//! It speaks the handshake every version of heli shares: `HELI` and the
//! protocol version as a little-endian u32, first from the client, then from
//! the daemon. Only [`Answer::Unsupported`] and [`Answer::Reply`] go
//! further.

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
    /// `HELI` and this build's protocol version, [`PROTOCOL`]; then it
    /// answers the client's request, whatever it is, as an older daemon
    /// answers a request it does not know: a message frame (kind 0, a
    /// little-endian u32 length) holding the JSON `"Unsupported"`.
    Unsupported,
    /// `HELI` and [`PROTOCOL`]; then it answers the client's request,
    /// whatever it is, with a message frame holding this JSON: what a daemon
    /// of another build of the same protocol would answer.
    Reply(&'static str),
    /// `HELI` and [`PROTOCOL`]; then it answers the client's request with a
    /// message frame for each of these JSON texts, in order, and keeps the
    /// connection open until the client closes it.
    Replies(&'static [&'static str]),
    /// As [`Answer::Replies`], then it hangs up at once: a daemon lost
    /// there.
    LostAfter(&'static [&'static str]),
}

/// The protocol version of this build of heli.
pub const PROTOCOL: u32 = 2;

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
                Answer::Unsupported
                | Answer::Reply(_)
                | Answer::Replies(_)
                | Answer::LostAfter(_) => {
                    let mut bytes = *b"HELI\0\0\0\0";
                    bytes[4..].copy_from_slice(&PROTOCOL.to_le_bytes());
                    bytes
                }
            };
            if stream.write_all(&bytes).is_err() {
                return Vec::new();
            }
            let replies: &[&str] = match &answer {
                Answer::Unsupported => &["\"Unsupported\""],
                Answer::Reply(json) => std::slice::from_ref(json),
                Answer::Replies(replies) | Answer::LostAfter(replies) => replies,
                _ => &[],
            };
            if !replies.is_empty() {
                let mut header = [0u8; 5];
                stream
                    .read_exact(&mut header)
                    .expect("the client's request");
                let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
                let mut request = vec![0u8; length as usize];
                stream
                    .read_exact(&mut request)
                    .expect("the client's request");
            }
            for reply in replies {
                let reply = reply.as_bytes();
                let mut frame = vec![0u8];
                frame.extend((reply.len() as u32).to_le_bytes());
                frame.extend(reply);
                if stream.write_all(&frame).is_err() {
                    return Vec::new();
                }
            }
            if matches!(answer, Answer::LostAfter(_)) {
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
///
/// On Unix, a socket left at `endpoint` (by an earlier fake, or a killed
/// daemon) is removed first, as the daemon does: a listener leaves its
/// socket behind.
#[cfg(unix)]
fn listen(endpoint: &OsStr) -> impl FnOnce() -> std::os::unix::net::UnixStream + Send + 'static {
    use std::os::unix::fs::FileTypeExt;

    if std::fs::symlink_metadata(endpoint).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        std::fs::remove_file(endpoint).expect("remove the socket left at the endpoint");
    }
    let listener =
        std::os::unix::net::UnixListener::bind(endpoint).expect("bind the fake daemon's socket");
    move || listener.accept().expect("accept a client").0
}

/// Starts listening at `endpoint` now (so a client cannot miss it), and
/// returns how to accept the one client.
///
/// On Windows, the pipe is the user's, as a daemon's is: a client trusts
/// only a pipe its own user owns, and an elevated process (a CI runner's)
/// would otherwise give it to the Administrators group.
#[cfg(windows)]
fn listen(endpoint: &OsStr) -> impl FnOnce() -> std::fs::File + Send + 'static {
    use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError};
    use windows_sys::Win32::System::Pipes::ConnectNamedPipe;

    let file = super::endpoint::pipe_owned_by(endpoint, &super::endpoint::current_user_sid())
        .expect("create the fake daemon's pipe");
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
