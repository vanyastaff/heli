//! The run log: the daemon's record of each run's output, which
//! `cargo heli log <run>` prints whole or in part (docs/runs.md, "Output").
//!
//! **Records.** One file per run, `<run>.log` in `Locations::run_logs`
//! (`runs/` in the state directory), written by the daemon as the output
//! arrives: a [`Recorder`], fed by `daemon::execute::pump`, the one place
//! all of a run's output passes. Output is recorded whether or not a client
//! still listens, and clients following the run read it from the record
//! (`daemon::live`). A record holds the run's stdout and stderr as they reached
//! the daemon: the exact bytes, in order within each stream; across the two
//! streams, in the order the daemon read them (two pipes have no common
//! order finer than that). Runs heli executes directly, without a daemon,
//! have no record.
//!
//! **Format.** The 8 bytes `HELI-LOG` and the format [`VERSION`] (`u32`,
//! little-endian), then events: a kind byte, a little-endian `u32` length,
//! then that many bytes. Kinds: [`STDOUT`] and [`STDERR`], output as one
//! read from the run's pipe returned it; [`START`], the record's first event
//! when there is one, the run's command line as JSON ([`Start`]). Readers
//! skip kinds they do not know, so new events need no new version.
//! An output event's **index** is its position among the record's output
//! events, from 0, as the daemon published it to the run's followers
//! (`daemon::live`); [`Event`] and [`select::Line`] carry it, so a selector
//! can relate what the two streams printed. A record can be read while it
//! is written: a reader sees every event written so far, and a partial
//! event at the end is not one yet.
//!
//! **Reading.** [`read`] opens a record and writes the part a selector
//! picks ([`select`], where the module docs say how to add one), after a
//! line naming the run it reruns, if it is a rerun ([`Start::rerun_of`]).
//! The daemon serves `log` requests with it; a client reads the record
//! itself when no daemon answers. [`start`] reads a record's start event,
//! for `cargo heli rerun` (`crate::rerun`).
//!
//! **Retention** (`[log]` in `crate::config`). When a run starts, its record
//! is created and the oldest records are deleted first until at most
//! [`Limits::runs`] remain, the new one included, and the others take at
//! most [`Limits::bytes`]. Records of runs in flight, and records that
//! clients following a run read ([`RunLog::pin`]), are never deleted.
//! Oldest means lowest run number: run ids only grow (`daemon::registry`).

pub mod select;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::class::Class;
use crate::config::LogConfig;
use crate::protocol::{Channel, Text};
use crate::run_id::RunId;

use select::{Out, Select};

/// The record format's version.
pub const VERSION: u32 = 1;

/// Event kind: bytes the run wrote to its stdout.
pub const STDOUT: u8 = 1;
/// Event kind: bytes the run wrote to its stderr.
pub const STDERR: u8 = 2;
/// Event kind: the run's command line ([`Start`]).
pub const START: u8 = 3;

/// A run's start event: what `cargo heli rerun` runs again. Never the
/// environment, which may hold secrets: a rerun takes the environment it is
/// given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// heli's command line as the user typed it, after `cargo heli`: heli's
    /// own flags, then the subcommand and its arguments, before the client
    /// added anything for its output mode (`crate::output::plan`).
    pub args: Vec<Text>,
    /// The client's working directory.
    pub cwd: Text,
    pub class: Class,
    /// The run this one reruns, `r<number>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerun_of: Option<String>,
}

const MAGIC: [u8; 8] = *b"HELI-LOG";

/// How many records the daemon keeps, and how many bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub runs: usize,
    pub bytes: u64,
}

impl Limits {
    /// The last 200 runs, 2 GiB at most.
    pub const DEFAULT: Self = Self {
        runs: 200,
        bytes: 2 << 30,
    };

    /// The limits `[log]` in the configuration sets; one it leaves out keeps
    /// its default.
    pub fn configured(config: &LogConfig) -> Self {
        Self {
            runs: config.runs.unwrap_or(Self::DEFAULT.runs),
            bytes: config.bytes.unwrap_or(Self::DEFAULT.bytes),
        }
    }
}

/// The run log of a daemon: where records go, and which are in use.
pub struct RunLog {
    dir: PathBuf,
    limits: Limits,
    /// Runs whose records are being written or read by clients following
    /// them, with how many uses each: retention skips them.
    live: Mutex<BTreeMap<u64, usize>>,
}

impl RunLog {
    pub fn new(dir: PathBuf, limits: Limits) -> Self {
        Self {
            dir,
            limits,
            live: Mutex::new(BTreeMap::new()),
        }
    }

    /// Keeps the record of `run`, whether it exists yet or not, from being
    /// pruned until the pin drops: for a client following the run
    /// (`daemon::live`).
    pub fn pin(&self, run: RunId) -> Pin<'_> {
        let number = run.number();
        self.hold(number);
        Pin { log: self, number }
    }

    fn hold(&self, number: u64) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        *live.entry(number).or_default() += 1;
    }

    fn release(&self, number: u64) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(uses) = live.get_mut(&number) {
            *uses -= 1;
            if *uses == 0 {
                live.remove(&number);
            }
        }
    }

    /// The directory of the records.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Starts the record of `run`, after pruning the oldest
    /// records to make room for it (see the module docs). A record that
    /// cannot be written never fails the run: the recorder then drops the
    /// output and keeps the error for the daemon's log.
    pub fn record(&self, run: RunId) -> Recorder<'_> {
        let number = run.number();
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        self.prune(&live);
        let file = std::fs::create_dir_all(&self.dir).and_then(|()| {
            let mut file = File::create(record_path(&self.dir, run))?;
            let mut header = MAGIC.to_vec();
            header.extend(VERSION.to_le_bytes());
            file.write_all(&header)?;
            Ok(file)
        });
        *live.entry(number).or_default() += 1;
        drop(live);
        let (file, error) = match file {
            Ok(file) => (Some(file), None),
            Err(error) => (None, Some(error)),
        };
        Recorder {
            log: self,
            number,
            file,
            error,
        }
    }

    /// Deletes the oldest records, except those in use (`live`), until
    /// there is room for one more within the limits.
    fn prune(&self, live: &BTreeMap<u64, usize>) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut records: Vec<(u64, PathBuf, u64)> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name();
                let run: RunId = name.to_str()?.strip_suffix(".log")?.parse().ok()?;
                let number = run.number();
                Some((number, entry.path(), entry.metadata().ok()?.len()))
            })
            .collect();
        records.sort_unstable_by_key(|&(number, ..)| number);
        let mut count = records.len();
        let mut bytes: u64 = records.iter().map(|&(.., size)| size).sum();
        for (number, path, size) in records {
            if count < self.limits.runs && bytes <= self.limits.bytes {
                break;
            }
            if !live.contains_key(&number) && std::fs::remove_file(path).is_ok() {
                count -= 1;
                bytes -= size;
            }
        }
    }
}

/// Writes one run's record.
pub struct Recorder<'a> {
    log: &'a RunLog,
    number: u64,
    /// `None` once recording has failed.
    file: Option<File>,
    error: Option<io::Error>,
}

impl Recorder<'_> {
    /// Records the run's command line, as the record's first event.
    pub fn start(&mut self, start: &Start) {
        let json = serde_json::to_vec(start).expect("BUG: a start event serializes");
        self.event(START, &json);
    }

    /// Appends what the run wrote to `channel`; `false` when it is not in
    /// the record (recording has failed).
    pub fn output(&mut self, channel: Channel, bytes: &[u8]) -> bool {
        let kind = match channel {
            Channel::Stdout => STDOUT,
            Channel::Stderr => STDERR,
        };
        self.event(kind, bytes)
    }

    fn event(&mut self, kind: u8, bytes: &[u8]) -> bool {
        let Some(file) = &mut self.file else {
            return false;
        };
        // Pipe reads are far smaller (`daemon::execute`).
        let length = u32::try_from(bytes.len()).expect("BUG: an event of 4 GiB");
        let mut event = Vec::with_capacity(5 + bytes.len());
        event.push(kind);
        event.extend(length.to_le_bytes());
        event.extend(bytes);
        match file.write_all(&event) {
            Ok(()) => true,
            Err(error) => {
                self.file = None;
                self.error = Some(error);
                false
            }
        }
    }

    /// Why the run is not (or no longer) recorded, once.
    pub fn take_error(&mut self) -> Option<io::Error> {
        self.error.take()
    }
}

impl Drop for Recorder<'_> {
    fn drop(&mut self) {
        self.log.release(self.number);
    }
}

/// Keeps a record from being pruned ([`RunLog::pin`]).
pub struct Pin<'a> {
    log: &'a RunLog,
    number: u64,
}

impl Drop for Pin<'_> {
    fn drop(&mut self) {
        self.log.release(self.number);
    }
}

/// One output event of a record.
#[derive(Debug)]
pub struct Event {
    /// Its position among the record's output events, from 0.
    pub index: u64,
    pub channel: Channel,
    pub bytes: Vec<u8>,
}

/// The output events of a record, in order.
pub struct Events {
    /// `None` for a record whose header is not written yet: no events.
    reader: Option<BufReader<File>>,
    /// The index of the next output event.
    next: u64,
}

impl Events {
    /// The next event of any kind: its kind and its bytes.
    fn next_raw(&mut self) -> Option<io::Result<(u8, Vec<u8>)>> {
        let reader = self.reader.as_mut()?;
        let mut header = [0u8; 5];
        match read_whole(reader, &mut header) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => return Some(Err(error)),
        }
        let mut length = [0u8; 4];
        length.copy_from_slice(&header[1..]);
        let length = u32::from_le_bytes(length);
        // Read through `take`, so a damaged length costs no more memory
        // than the file holds.
        let mut bytes = Vec::new();
        match reader.by_ref().take(length.into()).read_to_end(&mut bytes) {
            Ok(read) if read == length as usize => {}
            Ok(_) => return None,
            Err(error) => return Some(Err(error)),
        }
        Some(Ok((header[0], bytes)))
    }
}

impl Iterator for Events {
    type Item = io::Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (kind, bytes) = match self.next_raw()? {
                Ok(event) => event,
                Err(error) => return Some(Err(error)),
            };
            let channel = match kind {
                STDOUT => Channel::Stdout,
                STDERR => Channel::Stderr,
                _ => continue,
            };
            let index = self.next;
            self.next += 1;
            return Some(Ok(Event {
                index,
                channel,
                bytes,
            }));
        }
    }
}

/// Fills `buffer`; `false` when the input ends first (a partial event is
/// not one yet).
fn read_whole(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => return Ok(false),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

/// Why a record cannot be read.
#[derive(Debug)]
pub enum ReadError {
    /// No record of this run: it never ran, or its record was pruned.
    Unknown(RunId),
    /// Not a run id: no run has a record.
    NotARun(String),
    /// The file is not a record this heli can read.
    Foreign(RunId),
    Io(RunId, io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(run) => write!(
                f,
                "no run {run} in the run log: no such run, or its record was pruned"
            ),
            Self::NotARun(run) => write!(
                f,
                "no run {run} in the run log: no such run, or its record was pruned"
            ),
            Self::Foreign(run) => {
                write!(f, "the record of {run} is not a run log this heli can read")
            }
            Self::Io(run, error) => write!(f, "cannot read the record of {run}: {error}"),
        }
    }
}

/// Opens the record of `run` in `dir`.
pub fn open(dir: &Path, run: RunId) -> Result<Events, ReadError> {
    let file = match File::open(record_path(dir, run)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ReadError::Unknown(run));
        }
        Err(error) => return Err(ReadError::Io(run, error)),
    };
    let mut reader = BufReader::new(file);
    let mut header = [0u8; 12];
    let mut expected = MAGIC.to_vec();
    expected.extend(VERSION.to_le_bytes());
    let reader = match read_whole(&mut reader, &mut header) {
        Ok(true) if header[..] == expected[..] => Some(reader),
        Ok(true) => return Err(ReadError::Foreign(run)),
        // The run has only just started: nothing recorded yet.
        Ok(false) => None,
        Err(error) => return Err(ReadError::Io(run, error)),
    };
    Ok(Events { reader, next: 0 })
}

/// The start event of `run`'s record in `dir`; `None` for a record without
/// one (written before heli recorded them, or not yet).
pub fn start(dir: &Path, run: RunId) -> Result<Option<Start>, ReadError> {
    let mut events = open(dir, run)?;
    match events.next_raw() {
        Some(Ok((START, json))) => Ok(serde_json::from_slice(&json).ok()),
        Some(Err(error)) => Err(ReadError::Io(run, error)),
        _ => Ok(None),
    }
}

/// Writes the part of `run`'s record in `dir` that `select` picks to `out`,
/// each piece on the channel it was written to. Unless `select` asks for
/// the record byte for byte, a rerun's begins with a line on stderr naming
/// the run it reruns.
pub fn read(dir: &Path, run: RunId, select: &Select, out: &mut Out<'_>) -> Result<(), ReadError> {
    if *select != Select::Raw
        && let Some(of) = start(dir, run)?.and_then(|start| start.rerun_of)
    {
        let header = format!("cargo-heli: {run}: rerun of {of}\n");
        out(Channel::Stderr, header.as_bytes()).map_err(|error| ReadError::Io(run, error))?;
    }
    let events = open(dir, run)?;
    select::apply(select, run, events, out).map_err(|error| ReadError::Io(run, error))
}

/// Writes what `select` picks from a record without output to `out`: that
/// of a run listed before its record begins (it waits for its turn), which
/// has printed nothing yet.
pub fn read_unstarted(run: RunId, select: &Select, out: &mut Out<'_>) -> Result<(), ReadError> {
    let events = Events {
        reader: None,
        next: 0,
    };
    select::apply(select, run, events, out).map_err(|error| ReadError::Io(run, error))
}

fn record_path(dir: &Path, run: RunId) -> PathBuf {
    dir.join(format!("{run}.log"))
}
