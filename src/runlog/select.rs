//! Selectors: which part of a record `cargo heli log` prints.
//!
//! Without one, `log` prints the record as a person would have seen it, each
//! line on its own stream ([`rendered`]): cargo's JSON messages, which heli
//! asks for in compact and quiet mode, become the diagnostics they carry as
//! rustc renders them, on stderr, once each, and its artifact and
//! build-script records are left out; every other line is as it was
//! recorded. A record that held cargo's JSON ends with one line on stderr
//! saying what it said ([`Summary`]): `cargo-heli: r17: no diagnostics; 15
//! artifacts (1 rebuilt); --raw for cargo's messages`, so that a clean run
//! reads as what it did, not as nothing (#123). `--tail` and `--grep` work
//! on these lines. `--raw` prints every output event as it was recorded
//! instead, byte for byte.
//!
//! **Adding a selector**:
//!
//! 1. A variant of [`Select`]. It travels in the `log` request
//!    (`crate::protocol`); a daemon that does not know it answers that it
//!    does not support the request, and the client then reads the record
//!    itself.
//! 2. Its flag in `crate::cli`, in the `select` group, so that selectors
//!    exclude each other.
//! 3. Its case in [`apply`]: a function over the record's [`Event`]s, or
//!    over its [`Lines`], that writes what it picks to `out`.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::io;
use std::rc::Rc;

use regex::bytes::Regex;
use serde::{Deserialize, Serialize};

use super::Event;
use crate::output::cargo::Message;
use crate::output::{libtest, nextest};
use crate::protocol::Channel;
use crate::run_id::RunId;

/// Which part of a record to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Select {
    /// All of it, as a person would have seen it ([`rendered`]).
    All,
    /// All of it, byte for byte, cargo's JSON messages included.
    Raw,
    /// The last this many lines.
    Tail(usize),
    /// The lines that match this regular expression (`regex` syntax).
    Grep(String),
    /// The errors, as the compiler rendered them ([`errors`]).
    Errors,
    /// The output of the test of this name ([`test()`]).
    Test(String),
}

/// Where a selector writes: bytes, and the stream they go to.
pub type Out<'a> = dyn FnMut(Channel, &[u8]) -> io::Result<()> + 'a;

/// Writes the part of `events`, the record of run `run`, that `select`
/// picks to `out`.
pub fn apply(
    select: &Select,
    run: RunId,
    events: impl Iterator<Item = io::Result<Event>>,
    out: &mut Out<'_>,
) -> io::Result<()> {
    match select {
        Select::Raw => {
            for event in events {
                let event = event?;
                out(event.channel, &event.bytes)?;
            }
            Ok(())
        }
        Select::All => rendered(run, Lines::new(events))
            .try_for_each(|line| line.and_then(|line| out(line.channel, &line.bytes))),
        Select::Tail(count) => tail(*count, rendered(run, Lines::new(events)), out),
        Select::Grep(pattern) => {
            let pattern = Regex::new(pattern).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad pattern: {error}"))
            })?;
            grep(&pattern, rendered(run, Lines::new(events)), out)
        }
        Select::Errors => errors(Lines::new(events), out),
        Select::Test(name) => {
            if !test(name, Lines::new(events), out)? {
                let note = format!("cargo-heli: no output of test `{name}` in {run}\n");
                out(Channel::Stderr, note.as_bytes())?;
            }
            Ok(())
        }
    }
}

/// The output of test `name` in a record, as its runner wrote it: under
/// libtest, on stdout, its section, from `---- <name> stdout ----` to the
/// next section or the end of the list (`crate::output::libtest`); under
/// nextest, on stderr, its status line and the output under it, to the
/// next status line or line of nextest's own (`crate::output::nextest`).
/// `name` is the test's path (`tests::it_works`), or for nextest also its
/// binary and path (`my-crate tests::it_works`). Blank lines that end an
/// output are left out.
///
/// nextest may print a test's status line twice, once without its output
/// (with `--failure-output final`, the output comes after the summary):
/// outputs with more than their status line come first, and when there is
/// none, the status lines are the output. Returns whether there was any.
fn test(
    name: &str,
    lines: impl Iterator<Item = io::Result<Line>>,
    out: &mut Out<'_>,
) -> io::Result<bool> {
    let mut outputs: Vec<Vec<Line>> = Vec::new();
    // The output being read, per stream: libtest's on stdout, nextest's on
    // stderr.
    let mut open: [Option<Vec<Line>>; 2] = [None, None];
    for line in lines {
        let line = line?;
        let text = String::from_utf8_lossy(&line.bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        let (starts, ends) = match line.channel {
            Channel::Stdout => (
                libtest::section_of(text) == Some(name),
                libtest::ends_section(text),
            ),
            Channel::Stderr => (
                nextest::is_status_of(text, name),
                nextest::ends_output(text),
            ),
        };
        let current = &mut open[slot(line.channel)];
        if let Some(output) = current
            && !ends
        {
            output.push(line);
            continue;
        }
        outputs.extend(current.take());
        if starts {
            *current = Some(vec![line]);
        }
    }
    outputs.extend(open.into_iter().flatten());
    for output in &mut outputs {
        while output
            .last()
            .is_some_and(|line| line.bytes.trim_ascii().is_empty())
        {
            output.pop();
        }
    }
    let full: Vec<&Vec<Line>> = outputs.iter().filter(|output| output.len() > 1).collect();
    let shown = if full.is_empty() {
        outputs.iter().collect()
    } else {
        full
    };
    for line in shown.iter().flat_map(|output| output.iter()) {
        out(line.channel, &line.bytes)?;
    }
    Ok(!shown.is_empty())
}

/// The errors in a record, on stderr, where cargo prints them:
///
/// - each error cargo reported as JSON on stdout (`--message-format json`,
///   so every compact or quiet run of build, check, clippy and doc), as
///   rustc rendered it, once however many targets reported it;
/// - each error in text on stderr (a run whose output passed through): from
///   a line that starts with `error:` or `error[` to the blank line that
///   ends it, or to the next warning or the end of the record. That covers
///   rustc's rendering and cargo's own one-line errors.
fn errors(lines: impl Iterator<Item = io::Result<Line>>, out: &mut Out<'_>) -> io::Result<()> {
    let mut seen = HashSet::new();
    let mut in_error = false;
    let ansi = Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("a constant ANSI pattern");
    for line in lines {
        let line = line?;
        if line.channel == Channel::Stdout {
            if let Some(Message::Diagnostic(diagnostic)) = Message::parse(&line.bytes)
                && diagnostic.is_error()
                && seen.insert(diagnostic.identity().to_owned())
            {
                out(Channel::Stderr, diagnostic.rendered().as_bytes())?;
            }
            continue;
        }
        // Classify the visible text, but return the original rendering.
        let plain = ansi.replace_all(&line.bytes, &b""[..]);
        let text = plain.trim_ascii_end();
        if text.starts_with(b"error:") || text.starts_with(b"error[") {
            in_error = true;
        } else if text.starts_with(b"warning") {
            in_error = false;
        }
        if in_error {
            out(Channel::Stderr, &line.bytes)?;
            in_error = !text.is_empty();
        }
    }
    Ok(())
}

/// `lines` as a person would have seen the run `run`: a line of stdout
/// that is one of cargo's JSON messages becomes the diagnostic it carries,
/// as rustc renders it, on stderr, where cargo prints it (each diagnostic
/// once, as cargo prints a repeated one once); its artifact, build-script
/// and build-finished records are left out, and what they said ends the
/// view as one line on stderr ([`Summary::line`]). Every other line is as
/// recorded.
fn rendered(
    run: RunId,
    lines: impl Iterator<Item = io::Result<Line>>,
) -> impl Iterator<Item = io::Result<Line>> {
    let summary = Rc::new(RefCell::new(Summary::default()));
    let counted = Rc::clone(&summary);
    let mut seen = HashSet::new();
    let body = lines.flat_map(move |line| {
        let line = match line {
            Ok(line) => line,
            Err(error) => return vec![Err(error)],
        };
        if line.channel != Channel::Stdout {
            return vec![Ok(line)];
        }
        let Some(message) = Message::parse(&line.bytes) else {
            return vec![Ok(line)];
        };
        let mut summary = counted.borrow_mut();
        summary.json = true;
        summary.last_event = line.event;
        match message {
            Message::Artifact { fresh } => {
                summary.artifacts += 1;
                summary.rebuilt += usize::from(!fresh);
                Vec::new()
            }
            Message::BuildFinished { success } => {
                summary.finished = Some(success);
                Vec::new()
            }
            Message::Other => Vec::new(),
            Message::Diagnostic(diagnostic) => {
                if !seen.insert(diagnostic.identity().to_owned()) {
                    return Vec::new();
                }
                summary.errors += usize::from(diagnostic.is_error());
                summary.warnings += usize::from(diagnostic.is_warning());
                // One line each, so that `--tail` counts and `--grep`
                // matches the lines a person reads.
                diagnostic
                    .rendered()
                    .split_inclusive('\n')
                    .map(|text| {
                        Ok(Line {
                            channel: Channel::Stderr,
                            event: line.event,
                            bytes: text.as_bytes().to_vec(),
                        })
                    })
                    .collect()
            }
        }
    });
    // Chained, so that the line comes once every line before it has been
    // counted.
    body.chain(std::iter::once_with(move || summary.borrow().line(run).map(Ok)).flatten())
}

/// What a record's cargo JSON said besides the diagnostics [`rendered`]
/// shows: the line that ends the rendered view of a compact or quiet run.
#[derive(Default)]
struct Summary {
    /// The record held cargo's JSON at all: the run asked for it.
    json: bool,
    /// Units cargo reported, and those among them it rebuilt.
    artifacts: usize,
    rebuilt: usize,
    /// Diagnostics shown, each once.
    errors: usize,
    warnings: usize,
    /// Whether the build succeeded, when cargo said it had ended.
    finished: Option<bool>,
    /// The event of the last JSON message.
    last_event: u64,
}

impl Summary {
    /// `cargo-heli: r17: no diagnostics; 15 artifacts (1 rebuilt); --raw
    /// for cargo's messages`, on stderr; `build failed` after the artifacts
    /// when cargo reported a failure without a diagnostic (a build script,
    /// the linker), `build not finished` when it reported no end (the run
    /// is in flight, or was cancelled). None when the record held no JSON.
    fn line(&self, run: RunId) -> Option<Line> {
        if !self.json {
            return None;
        }
        let diagnostics = match (self.errors, self.warnings) {
            (0, 0) => "no diagnostics".to_owned(),
            (errors, 0) => count(errors, "error"),
            (0, warnings) => count(warnings, "warning"),
            (errors, warnings) => {
                format!("{}, {}", count(errors, "error"), count(warnings, "warning"))
            }
        };
        let artifacts = match (self.artifacts, self.rebuilt) {
            (0, _) => "no artifacts".to_owned(),
            (artifacts, 0) => format!("{} (all fresh)", count(artifacts, "artifact")),
            (artifacts, rebuilt) => format!("{} ({rebuilt} rebuilt)", count(artifacts, "artifact")),
        };
        let mut text = format!("cargo-heli: {run}: {diagnostics}; {artifacts}");
        match self.finished {
            Some(true) => {}
            Some(false) if self.errors == 0 => text.push_str("; build failed"),
            Some(false) => {}
            None => text.push_str("; build not finished"),
        }
        text.push_str("; --raw for cargo's messages\n");
        Some(Line {
            channel: Channel::Stderr,
            event: self.last_event,
            bytes: text.into_bytes(),
        })
    }
}

/// `1 error`, `2 errors`.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The lines that match `pattern`, without their line ends.
fn grep(
    pattern: &Regex,
    lines: impl Iterator<Item = io::Result<Line>>,
    out: &mut Out<'_>,
) -> io::Result<()> {
    for line in lines {
        let line = line?;
        let text = line.bytes.strip_suffix(b"\n").unwrap_or(&line.bytes);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if pattern.is_match(text) {
            out(line.channel, &line.bytes)?;
        }
    }
    Ok(())
}

/// The last `count` lines.
fn tail(
    count: usize,
    lines: impl Iterator<Item = io::Result<Line>>,
    out: &mut Out<'_>,
) -> io::Result<()> {
    let mut last = VecDeque::with_capacity(count.min(4096));
    for line in lines {
        last.push_back(line?);
        if last.len() > count {
            last.pop_front();
        }
    }
    last.iter()
        .try_for_each(|line| out(line.channel, &line.bytes))
}

/// One line of a record: one stream's bytes up to and including a `\n`, or
/// the stream's last bytes, when they do not end a line.
#[derive(Debug)]
pub struct Line {
    pub channel: Channel,
    /// The index of the event that holds its last byte.
    pub event: u64,
    pub bytes: Vec<u8>,
}

/// The lines of a record's events, in the order their last bytes were
/// recorded.
pub struct Lines<I> {
    events: I,
    /// Per stream (stdout, stderr): the start of a line, and the event
    /// holding its last byte so far.
    pending: [(Vec<u8>, u64); 2],
    ready: VecDeque<Line>,
    done: bool,
}

impl<I: Iterator<Item = io::Result<Event>>> Lines<I> {
    pub fn new(events: I) -> Self {
        Self {
            events,
            pending: [(Vec::new(), 0), (Vec::new(), 0)],
            ready: VecDeque::new(),
            done: false,
        }
    }

    fn split(&mut self, event: Event) {
        let (pending, last_event) = &mut self.pending[slot(event.channel)];
        let mut start = 0;
        for (end, _) in event
            .bytes
            .iter()
            .enumerate()
            .filter(|&(_, &byte)| byte == b'\n')
        {
            pending.extend_from_slice(&event.bytes[start..=end]);
            self.ready.push_back(Line {
                channel: event.channel,
                event: event.index,
                bytes: std::mem::take(pending),
            });
            start = end + 1;
        }
        if start < event.bytes.len() {
            pending.extend_from_slice(&event.bytes[start..]);
            *last_event = event.index;
        }
    }

    /// The streams' unended last lines, the one recorded first first.
    fn finish(&mut self) {
        let mut unended: Vec<Line> = [Channel::Stdout, Channel::Stderr]
            .into_iter()
            .filter_map(|channel| {
                let (bytes, event) = std::mem::take(&mut self.pending[slot(channel)]);
                (!bytes.is_empty()).then_some(Line {
                    channel,
                    event,
                    bytes,
                })
            })
            .collect();
        unended.sort_by_key(|line| line.event);
        self.ready.extend(unended);
    }
}

impl<I: Iterator<Item = io::Result<Event>>> Iterator for Lines<I> {
    type Item = io::Result<Line>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(line) = self.ready.pop_front() {
                return Some(Ok(line));
            }
            if self.done {
                return None;
            }
            match self.events.next() {
                Some(Ok(event)) => self.split(event),
                Some(Err(error)) => {
                    self.done = true;
                    return Some(Err(error));
                }
                None => {
                    self.done = true;
                    self.finish();
                }
            }
        }
    }
}

fn slot(channel: Channel) -> usize {
    match channel {
        Channel::Stdout => 0,
        Channel::Stderr => 1,
    }
}
