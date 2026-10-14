//! Selectors: which part of a record `cargo heli log` prints.
//!
//! Without one, `log` prints every output event as it was recorded, each on
//! its own stream, so stdout and stderr come out byte for byte.
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

use std::collections::{HashSet, VecDeque};
use std::io;

use regex::bytes::Regex;
use serde::{Deserialize, Serialize};

use super::Event;
use crate::output::cargo::Message;
use crate::output::{libtest, nextest};
use crate::protocol::Channel;

/// Which part of a record to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Select {
    /// All of it, byte for byte.
    All,
    /// The last this many lines.
    Tail(usize),
    /// The lines that match this regular expression (`regex` syntax).
    Grep(String),
    /// The errors, as the compiler rendered them ([`errors`]).
    Errors,
    /// The output of the test of this name ([`test`]).
    Test(String),
}

/// Where a selector writes: bytes, and the stream they go to.
pub type Out<'a> = dyn FnMut(Channel, &[u8]) -> io::Result<()> + 'a;

/// Writes the part of `events`, the record of run `run`, that `select`
/// picks to `out`.
pub fn apply(
    select: &Select,
    run: &str,
    events: impl Iterator<Item = io::Result<Event>>,
    out: &mut Out<'_>,
) -> io::Result<()> {
    match select {
        Select::All => {
            for event in events {
                let event = event?;
                out(event.channel, &event.bytes)?;
            }
            Ok(())
        }
        Select::Tail(count) => tail(*count, Lines::new(events), out),
        Select::Grep(pattern) => {
            let pattern = Regex::new(pattern).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad pattern: {error}"))
            })?;
            grep(&pattern, Lines::new(events), out)
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

/// Where a test's output is in a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runner {
    /// On stdout, from `---- <name> stdout ----`.
    Libtest,
    /// On stderr, from the test's status line.
    Nextest,
}

impl Runner {
    fn channel(self) -> Channel {
        match self {
            Self::Libtest => Channel::Stdout,
            Self::Nextest => Channel::Stderr,
        }
    }

    /// Whether `line` (without its line end) starts the output of test
    /// `name`.
    fn starts(self, line: &str, name: &str) -> bool {
        match self {
            Self::Libtest => libtest::section_of(line) == Some(name),
            Self::Nextest => nextest::status_line(line)
                .is_some_and(|test| test == name || test.ends_with(&format!(" {name}"))),
        }
    }

    /// Whether `line` (without its line end) ends a test's output.
    fn ends(self, line: &str) -> bool {
        match self {
            Self::Libtest => libtest::ends_section(line),
            Self::Nextest => nextest::ends_output(line),
        }
    }
}

/// The output of test `name` in a record, as its runner wrote it: under
/// libtest, its section, from `---- <name> stdout ----` to the next
/// section or the end of the list (`crate::output::libtest`); under
/// nextest, its status line and the output under it, to the next status
/// line, separator or line of nextest's own (`crate::output::nextest`).
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
    let mut open: Option<(Runner, Vec<Line>)> = None;
    for line in lines {
        let line = line?;
        let text = String::from_utf8_lossy(&line.bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        if let Some((runner, _)) = &open {
            if line.channel != runner.channel() {
                continue;
            }
            if !runner.ends(text) {
                open.as_mut().expect("an open output").1.push(line);
                continue;
            }
            outputs.extend(open.take().map(|(_, output)| output));
        }
        if let Some(runner) = [Runner::Libtest, Runner::Nextest]
            .into_iter()
            .find(|runner| line.channel == runner.channel() && runner.starts(text, name))
        {
            open = Some((runner, vec![line]));
        }
    }
    outputs.extend(open.map(|(_, output)| output));
    for output in &mut outputs {
        while output
            .last()
            .is_some_and(|line| output.len() > 1 && line.bytes.trim_ascii().is_empty())
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
        let text = line.bytes.trim_ascii_end();
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
