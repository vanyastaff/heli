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
//! - [`Request::Run`]: [`Reply::SpawnFailed`] for invalid execution inputs,
//!   [`Reply::Refused`] (the client then runs the command
//!   itself), or [`Reply::Started`], then [`Reply::Waiting`] if the run
//!   has to wait for its worktree's lock, then [`Reply::Spawned`] once its
//!   command has started (to a client that reads it, and then `Started`
//!   says it follows), then output frames in the order the run wrote them,
//!   then [`Reply::Exited`] or [`Reply::SpawnFailed`]. The daemon sends
//!   `Started` before it starts anything, so a client that never sees it can
//!   safely run the command itself, and so can one told that `Spawned`
//!   follows, until it sees that. Either before
//!   `Started` or in place of the output, [`Reply::Passed`] ends the run
//!   without starting it: the result store holds a pass for its key. A
//!   client that sends no [`RunRequest::class`] predates the worktree lock
//!   and is never sent `Waiting`, which it could not read. When the same
//!   run is already in flight (the same command, or for a verify or test
//!   run the same key, in any worktree), the client attaches to it instead
//!   (`Started` says so, and for a key names the run's worktree): the
//!   output frames then hold the run's output from its first byte, and the
//!   end is the run's.
//! - [`Request::Cancel`]: [`Reply::Cancelled`] once every process of the run
//!   is gone, or [`Reply::CancelFailed`].
//! - [`Request::Log`]: output frames holding the part of the run's record
//!   asked for (`crate::runlog`), each on the channel the run wrote it to,
//!   then [`Reply::LogEnd`]; or [`Reply::LogFailed`].
//! - A request the daemon does not know (it is older than the client):
//!   [`Reply::Unsupported`]. The client treats it like a version mismatch:
//!   it sends [`RETIRE`], and the daemon retires as above; then the client
//!   does without the daemon (it runs the command, or reads the run log,
//!   itself).
//!
//! **Changes.** Version 2 requires dependency-epoch ownership: a version 1
//! daemon would ignore that added request field and run without its lease.
//! A new request otherwise needs no new version: older daemons answer it with
//! `Unsupported`. Fields are only added, with defaults, so that both sides
//! read each other's messages, and a value a newer side adds to an enum
//! reads as unknown on an older one: a reply ([`Reply::Unknown`], which a
//! client following a run skips), a run's phase or class in a status
//! (`crate::status`). A daemon sends a new reply only to a client that
//! says it reads it. A change that breaks that raises [`VERSION`].
//!
//! JSON (serde, serde_json) keeps messages readable and easy to extend for
//! the requests later tickets add; output stays raw bytes in frames of its
//! own, since it need not be UTF-8.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::class::Class;
use crate::environment;
use crate::run_id::RunId;
use crate::runlog::select::Select;
use crate::status::{self, Status};

/// This build's protocol version.
pub const VERSION: u32 = 2;

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
    /// Kill a run in flight, with every process it started.
    Cancel(CancelRequest),
}

/// The run to cancel.
#[derive(Debug, Serialize, Deserialize)]
pub struct CancelRequest {
    /// `r<number>`, as the client was given it: the daemon answers one
    /// that is not a run id as a run it does not know, and never takes it
    /// for a request it does not know.
    pub run: String,
}

/// The run whose record to send, and which part of it.
#[derive(Debug, Serialize, Deserialize)]
pub struct LogRequest {
    /// `r<number>`, as for [`CancelRequest::run`].
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
    /// The immutable dependency epoch selected by this execution context.
    /// The daemon validates it and owns its lease before inspecting inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_epoch: Option<Text>,
    /// What kind of run it is, as the client read its command line; `None`
    /// from a client older than run classes.
    #[serde(default)]
    pub class: Option<Class>,
    /// How the run uses the result store; absent from a client that knows
    /// no result store.
    #[serde(default)]
    pub cache: Cache,
    /// The client reads [`Reply::Spawned`]; absent (false) from a client
    /// older than it, which is never sent one.
    #[serde(default)]
    pub reads_spawned: bool,
}

impl RunRequest {
    /// The command line, program first.
    pub fn argv(&self) -> Vec<OsString> {
        self.argv.iter().map(|arg| arg.0.clone()).collect()
    }

    /// The working directory.
    pub fn cwd(&self) -> PathBuf {
        PathBuf::from(&self.cwd.0)
    }

    /// The client's variable `name`, by the platform's rule for names (on
    /// Windows without case); the last one given, should there be two.
    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.env
            .iter()
            .rev()
            .find(|(candidate, _)| environment::same_name(&candidate.0, name))
            .map(|(_, value)| value.0.as_os_str())
    }
}

/// How a run uses the result store (`crate::daemon::results`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cache {
    /// Neither looked up nor recorded: mutate and ordinary exec runs.
    #[default]
    Off,
    /// Legacy cache-off requests: no key, record or single-flight.
    /// Retained for clients predating source-keyed live sharing.
    Disabled,
    /// Source-keyed live sharing, without reading or changing saved verdicts.
    /// A separate wire value prevents older daemons from silently skipping
    /// the source-freshness contract of a new `--cache off` request.
    Live,
    /// Looked up first, and recorded when it passes: verify and test runs.
    On,
    /// Run whatever is recorded (`--no-cache`), and recorded when it passes.
    Refresh,
}

impl Cache {
    /// How a run of `class`, `cargo <args>`, uses the store, as the client
    /// decides: verify and test runs look it up and are recorded (with
    /// `no_cache`, only recorded), the result-store column of the class
    /// table (`crate::class`). A request for help (`-h` or `--help`,
    /// cargo's or, after `--`, the test runner's) is no verdict, so it does
    /// not use it. CLI `exec --verify` explicitly declares verification
    /// and selects its policy separately (live sharing by default).
    pub fn of(class: Class, args: &[OsString], no_cache: bool) -> Self {
        let help = args.iter().any(|arg| arg == "-h" || arg == "--help");
        match class {
            Class::Verify | Class::Test if help => Self::Off,
            Class::Verify | Class::Test if no_cache => Self::Refresh,
            Class::Verify | Class::Test => Self::On,
            Class::Compile if !help => Self::Live,
            Class::Compile | Class::Mutate | Class::Exec => Self::Off,
        }
    }
}

/// What the daemon answers.
#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    /// The daemon is preparing source keys or selected immutable inputs. No
    /// command has started; the client may remove its first-answer deadline.
    Preparing,
    /// The daemon accepted the run, and starts it now or once its wait
    /// ends; or, with `attached`, the same run was already in flight as
    /// `run`, and the client follows that run from its start.
    Started {
        run: RunId,
        /// For an attached client: how long the run had been going, in
        /// milliseconds.
        #[serde(default)]
        attached: Option<u64>,
        /// For a client attached by key (a verify or test run of the same
        /// key, single-flight): the run's worktree, which may be another
        /// than the client's.
        #[serde(default)]
        keyed_in: Option<Text>,
        /// [`Reply::Spawned`] will say when the run's command has started:
        /// until then, a client that loses the daemon may run the command
        /// itself. Absent (false) from a daemon older than `Spawned`, and
        /// for a client that does not read it.
        #[serde(default)]
        spawned_follows: bool,
    },
    /// The run waits for these runs, which use its worktree, before it
    /// starts (the worktree lock).
    Waiting {
        runs: Vec<status::Run>,
    },
    /// The run's command has started (the first process of its first
    /// phase): from here on its client never runs it itself. Sent only to a
    /// client whose request [`RunRequest::reads_spawned`].
    Spawned,
    /// The daemon takes no runs (it is retiring); nothing was started.
    Refused {
        reason: String,
    },
    /// The run's key already passed: nothing runs, and the client exits 0.
    /// `run` is the run that passed, in `worktree`, at `at` (seconds since
    /// 1970).
    Passed {
        run: RunId,
        worktree: Text,
        at: u64,
    },
    /// The command could not be started.
    SpawnFailed {
        not_found: bool,
        message: String,
    },
    /// The command exited. On Unix, `signal` is the signal that killed it,
    /// and `code` is then 128 plus that signal. `cancelled`: `cargo heli
    /// cancel` ended it.
    Exited {
        code: i32,
        signal: Option<i32>,
        #[serde(default)]
        cancelled: bool,
    },
    Status(Box<Status>),
    /// The daemon does not know the request: it is older than the client.
    Unsupported,
    /// The part of the record asked for has been sent.
    LogEnd,
    /// The record cannot be read; `message` says why.
    LogFailed {
        message: String,
    },
    /// The run was cancelled: none of its processes is left.
    Cancelled,
    /// The run was not cancelled; `message` says why.
    CancelFailed {
        message: String,
    },
    /// A message of a newer daemon's that this build does not know (a
    /// variant it lacks, or one whose fields it cannot read). Never sent.
    /// A client following a run skips it; elsewhere it is an answer the
    /// client did not expect.
    #[serde(untagged, skip_serializing)]
    Unknown(serde::de::IgnoredAny),
}

impl Reply {
    /// The reply that tells a client how its run ended.
    pub fn exited(exit: crate::status::Exit) -> Self {
        Self::Exited {
            code: exit.code,
            signal: exit.signal,
            cancelled: exit.cancelled,
        }
    }
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
