//! The run log: the daemon's record of each run's output, which
//! `cargo heli log <run>` prints whole or in part (`DESIGN.md` §5, "Output").
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
//! read from the run's pipe returned it. Readers skip kinds they do not
//! know, so new events (the run's start or end, say) need no new version.
//! An event's **index** is its position among the record's events, from 0;
//! [`Event`] and [`select::Line`] carry it, so a selector can relate what
//! the two streams printed. A record can be read while it is written: a
//! reader sees every event written so far, and a partial event at the end is
//! not one yet.
//!
//! **Reading.** [`read`] opens a record and writes the part a selector
//! picks ([`select`], where the module docs say how to add one). The daemon
//! serves `log` requests with it; a client reads the record itself when no
//! daemon answers.
//!
//! **Retention** (`[log]` in `crate::config`). When a run starts, its record
//! is created and the oldest records are deleted first until at most
//! [`Limits::runs`] remain, the new one included, and the others take at
//! most [`Limits::bytes`]. Records of
//! runs in flight are never deleted. Oldest means lowest run number: run
//! ids only grow (`daemon::registry`).

pub mod select;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::config::LogConfig;
use crate::protocol::Channel;

use select::{Out, Select};

/// The record format's version.
pub const VERSION: u32 = 1;

/// Event kind: bytes the run wrote to its stdout.
pub const STDOUT: u8 = 1;
/// Event kind: bytes the run wrote to its stderr.
pub const STDERR: u8 = 2;

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

/// The run log of a daemon: where records go, and which are being written.
pub struct RunLog {
    dir: PathBuf,
    limits: Limits,
    /// Runs whose records are being written: retention skips them.
    live: Mutex<BTreeSet<u64>>,
}

impl RunLog {
    pub fn new(dir: PathBuf, limits: Limits) -> Self {
        Self {
            dir,
            limits,
            live: Mutex::new(BTreeSet::new()),
        }
    }

    /// The directory of the records.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Starts the record of `run` (`r<number>`), after pruning the oldest
    /// records to make room for it (see the module docs). A record that
    /// cannot be written never fails the run: the recorder then drops the
    /// output and keeps the error for the daemon's log.
    pub fn record(&self, run: &str) -> Recorder<'_> {
        let number = run_number(run).expect("BUG: run ids are r<number>");
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        self.prune(&live);
        let file = std::fs::create_dir_all(&self.dir).and_then(|()| {
            let mut file = File::create(record_path(&self.dir, run))?;
            let mut header = MAGIC.to_vec();
            header.extend(VERSION.to_le_bytes());
            file.write_all(&header)?;
            Ok(file)
        });
        live.insert(number);
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

    /// Deletes the oldest records, except those of runs in flight (`live`),
    /// until there is room for one more within the limits.
    fn prune(&self, live: &BTreeSet<u64>) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut records: Vec<(u64, PathBuf, u64)> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name();
                let number = run_number(name.to_str()?.strip_suffix(".log")?)?;
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
            if !live.contains(&number) && std::fs::remove_file(path).is_ok() {
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
    /// Appends what the run wrote to `channel`; `false` when it is not in
    /// the record (recording has failed).
    pub fn output(&mut self, channel: Channel, bytes: &[u8]) -> bool {
        let Some(file) = &mut self.file else {
            return false;
        };
        let kind = match channel {
            Channel::Stdout => STDOUT,
            Channel::Stderr => STDERR,
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
        self.log
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.number);
    }
}

/// One output event of a record.
#[derive(Debug)]
pub struct Event {
    /// Its position among the record's events, from 0.
    pub index: u64,
    pub channel: Channel,
    pub bytes: Vec<u8>,
}

/// The output events of a record, in order.
pub struct Events {
    /// `None` for a record whose header is not written yet: no events.
    reader: Option<BufReader<File>>,
    next: u64,
}

impl Iterator for Events {
    type Item = io::Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        let reader = self.reader.as_mut()?;
        loop {
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
            let index = self.next;
            self.next += 1;
            let channel = match header[0] {
                STDOUT => Channel::Stdout,
                STDERR => Channel::Stderr,
                _ => continue,
            };
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
    Unknown(String),
    /// The file is not a record this heli can read.
    Foreign(String),
    Io(String, io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(run) => write!(
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
pub fn open(dir: &Path, run: &str) -> Result<Events, ReadError> {
    if run_number(run).is_none() {
        return Err(ReadError::Unknown(run.to_owned()));
    }
    let file = match File::open(record_path(dir, run)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ReadError::Unknown(run.to_owned()));
        }
        Err(error) => return Err(ReadError::Io(run.to_owned(), error)),
    };
    let mut reader = BufReader::new(file);
    let mut header = [0u8; 12];
    let mut expected = MAGIC.to_vec();
    expected.extend(VERSION.to_le_bytes());
    let reader = match read_whole(&mut reader, &mut header) {
        Ok(true) if header[..] == expected[..] => Some(reader),
        Ok(true) => return Err(ReadError::Foreign(run.to_owned())),
        // The run has only just started: nothing recorded yet.
        Ok(false) => None,
        Err(error) => return Err(ReadError::Io(run.to_owned(), error)),
    };
    Ok(Events { reader, next: 0 })
}

/// Writes the part of `run`'s record in `dir` that `select` picks to `out`,
/// each piece on the channel it was written to.
pub fn read(dir: &Path, run: &str, select: &Select, out: &mut Out<'_>) -> Result<(), ReadError> {
    let events = open(dir, run)?;
    select::apply(select, events, out).map_err(|error| ReadError::Io(run.to_owned(), error))
}

/// The number of run id `run` (`r<number>`).
pub fn run_number(run: &str) -> Option<u64> {
    let digits = run.strip_prefix('r')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn record_path(dir: &Path, run: &str) -> PathBuf {
    dir.join(format!("{run}.log"))
}
