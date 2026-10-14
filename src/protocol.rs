//! The protocol between `cargo heli` and its daemon, over one connection of
//! the owner-only transport (`crate::ipc`).
//!
//! **Handshake**, frozen across all versions so that any client and any
//! daemon can tell each other apart:
//!
//! 1. The client sends `HELI` and its protocol version (`u32`,
//!    little-endian); the daemon answers the same way with its own.
//! 2. If the versions differ, the conversation ends there. A client newer
//!    than the daemon first sends [`RETIRE`]: the daemon then accepts no new
//!    runs and exits once its runs have finished, so the next call starts a
//!    current daemon. An older client sends nothing, so two installed
//!    versions do not take turns retiring each other's daemon.
//!
//! **Frames**, after a matching handshake: a kind byte, a little-endian
//! `u32` length, then the payload. A message frame holds one JSON
//! [`Request`] or [`Reply`]; an output frame holds raw bytes the run wrote to
//! its stdout or stderr. The client sends one request; the daemon answers:
//!
//! - [`Request::Status`]: one [`Reply::Status`].
//! - [`Request::Run`]: [`Reply::Refused`] (the client then runs the command
//!   itself), or [`Reply::Started`], then [`Reply::Waiting`] if the run
//!   has to wait for its worktree's lock, then output frames in the order
//!   the run wrote them, then [`Reply::Exited`] or [`Reply::SpawnFailed`].
//!   The daemon sends `Started` before it starts anything, so a client that
//!   never sees it can safely run the command itself. Either before
//!   `Started` or in place of the output, [`Reply::Passed`] ends the run
//!   without starting it: the result store holds a pass for its key. A
//!   client that sends no [`RunRequest::class`] predates the worktree lock
//!   and is never sent `Waiting`, which it could not read.
//! - [`Request::Log`]: output frames holding the part of the run's record
//!   asked for (`crate::runlog`), each on the channel the run wrote it to,
//!   then [`Reply::LogEnd`]; or [`Reply::LogFailed`].
//! - A request the daemon does not know (it is older than the client):
//!   [`Reply::Unsupported`]. The client treats it like a version mismatch:
//!   it sends [`RETIRE`], and the daemon retires as above; then the client
//!   does without the daemon (it runs the command, or reads the run log,
//!   itself).
//!
//! **Changes.** The version stays 1 until the first release. Until then a
//! new request needs no new version: older daemons answer it with
//! `Unsupported`. Fields are only added, with defaults, so that both sides
//! read each other's messages. A change that breaks that raises
//! [`VERSION`].
//!
//! JSON (serde, serde_json) keeps messages readable and easy to extend for
//! the requests later tickets add; output stays raw bytes in frames of its
//! own, since it need not be UTF-8.

use std::ffi::OsString;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::class::Class;
use crate::runlog::select::Select;
use crate::status::{self, Status};

/// This build's protocol version.
pub const VERSION: u32 = 1;

/// Sent by a newer client after a version mismatch: exit once idle.
pub const RETIRE: u8 = b'R';

const MAGIC: [u8; 4] = *b"HELI";

/// The largest message frame accepted: a request carries the client's whole
/// environment, which stays far below this.
const MAX_MESSAGE: u32 = 16 << 20;

const MESSAGE: u8 = 0;
const STDOUT: u8 = 1;
const STDERR: u8 = 2;

/// Sends this side's half of the handshake.
pub fn write_hello(stream: &mut impl Write) -> io::Result<()> {
    let mut hello = [0u8; 8];
    hello[..4].copy_from_slice(&MAGIC);
    hello[4..].copy_from_slice(&VERSION.to_le_bytes());
    stream.write_all(&hello)?;
    stream.flush()
}

/// Reads the other side's half of the handshake: its protocol version.
pub fn read_hello(stream: &mut impl Read) -> io::Result<u32> {
    let mut hello = [0u8; 8];
    stream.read_exact(&mut hello)?;
    if hello[..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the other side is not heli",
        ));
    }
    let mut version = [0u8; 4];
    version.copy_from_slice(&hello[4..]);
    Ok(u32::from_le_bytes(version))
}

/// What a client asks.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Execute a command and stream its output back.
    Run(RunRequest),
    /// Describe the daemon and its runs.
    Status,
    /// Send part of a run's record (`crate::runlog`).
    Log(LogRequest),
}

/// The run whose record to send, and which part of it.
#[derive(Debug, Serialize, Deserialize)]
pub struct LogRequest {
    /// `r<number>`.
    pub run: String,
    pub select: Select,
}

/// A command to execute as the client would have: in its working directory,
/// with exactly its environment.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub cwd: Text,
    /// The program, then its arguments.
    pub argv: Vec<Text>,
    pub env: Vec<(Text, Text)>,
    /// What kind of run it is, as the client read its command line; `None`
    /// from a client older than run classes.
    #[serde(default)]
    pub class: Option<Class>,
    /// How the run uses the result store; absent from a client that knows
    /// no result store.
    #[serde(default)]
    pub cache: Cache,
}

/// How a run uses the result store (`crate::daemon::results`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cache {
    /// Neither looked up nor recorded: compile, mutate and exec runs.
    #[default]
    Off,
    /// Looked up first, and recorded when it passes: verify and test runs.
    On,
    /// Run whatever is recorded (`--no-cache`), and recorded when it passes.
    Refresh,
}

/// What the daemon answers.
#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    /// The daemon accepted the run, and starts it now or once its wait ends.
    Started {
        run: String,
    },
    /// The run waits for these runs, which use its worktree, before it
    /// starts (the worktree lock).
    Waiting {
        runs: Vec<status::Run>,
    },
    /// The daemon takes no runs (it is retiring); nothing was started.
    Refused {
        reason: String,
    },
    /// The run's key already passed: nothing runs, and the client exits 0.
    /// `run` is the run that passed, in `worktree`, at `at` (seconds since
    /// 1970).
    Passed {
        run: String,
        worktree: Text,
        at: u64,
    },
    /// The command could not be started.
    SpawnFailed {
        not_found: bool,
        message: String,
    },
    /// The command exited. On Unix, `signal` is the signal that killed it,
    /// and `code` is then 128 plus that signal.
    Exited {
        code: i32,
        signal: Option<i32>,
    },
    Status(Status),
    /// The daemon does not know the request: it is older than the client.
    Unsupported,
    /// The part of the record asked for has been sent.
    LogEnd,
    /// The record cannot be read; `message` says why.
    LogFailed {
        message: String,
    },
}

/// An output stream of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stdout,
    Stderr,
}

/// One frame received.
#[derive(Debug)]
pub enum Frame<T> {
    Message(T),
    Output(Channel, Vec<u8>),
}

/// Sends one message frame.
pub fn write_message(stream: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let payload = serde_json::to_vec(message).map_err(io::Error::other)?;
    write_frame(stream, MESSAGE, &payload)
}

/// Sends one output frame.
pub fn write_output(stream: &mut impl Write, channel: Channel, bytes: &[u8]) -> io::Result<()> {
    let kind = match channel {
        Channel::Stdout => STDOUT,
        Channel::Stderr => STDERR,
    };
    write_frame(stream, kind, bytes)
}

fn write_frame(stream: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&length.to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

/// Reads one frame; a message must be a `T`.
pub fn read_frame<T: DeserializeOwned>(stream: &mut impl Read) -> io::Result<Frame<T>> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let mut length = [0u8; 4];
    length.copy_from_slice(&header[1..]);
    let length = u32::from_le_bytes(length);
    let invalid = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
    if header[0] == MESSAGE && length > MAX_MESSAGE {
        return Err(invalid(format!("message of {length} bytes")));
    }
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload)?;
    match header[0] {
        MESSAGE => serde_json::from_slice(&payload)
            .map(Frame::Message)
            .map_err(|error| invalid(format!("bad message: {error}"))),
        STDOUT => Ok(Frame::Output(Channel::Stdout, payload)),
        STDERR => Ok(Frame::Output(Channel::Stderr, payload)),
        kind => Err(invalid(format!("unknown frame kind {kind}"))),
    }
}

/// Reads one frame that must be a message.
pub fn read_message<T: DeserializeOwned>(stream: &mut impl Read) -> io::Result<T> {
    match read_frame(stream)? {
        Frame::Message(message) => Ok(message),
        Frame::Output(..) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a message, got output",
        )),
    }
}

/// An `OsString` on the wire, without loss: a JSON string when it is valid
/// Unicode, else its native units (bytes on Unix, UTF-16 units on Windows).
/// Client and daemon run on the same machine, so the units mean the same to
/// both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text(pub OsString);

#[cfg(unix)]
type Unit = u8;
#[cfg(windows)]
type Unit = u16;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum TextRepr {
    Unicode(String),
    Native(Vec<Unit>),
}

impl Serialize for Text {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0.to_str() {
            Some(text) => TextRepr::Unicode(text.to_owned()),
            None => TextRepr::Native(native_units(&self.0)),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(match TextRepr::deserialize(deserializer)? {
            TextRepr::Unicode(text) => OsString::from(text),
            TextRepr::Native(units) => from_native_units(units),
        }))
    }
}

#[cfg(unix)]
fn native_units(text: &OsString) -> Vec<Unit> {
    use std::os::unix::ffi::OsStrExt;
    text.as_bytes().to_vec()
}

#[cfg(unix)]
fn from_native_units(units: Vec<Unit>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(units)
}

#[cfg(windows)]
fn native_units(text: &OsString) -> Vec<Unit> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().collect()
}

#[cfg(windows)]
fn from_native_units(units: Vec<Unit>) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(&units)
}
