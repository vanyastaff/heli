//! `cargo heli exec`, compact: the view of an arbitrary command's output in
//! compact and quiet mode (`DESIGN.md` §5, "Output").
//!
//! heli knows nothing of the command, so it adds nothing to it, and shows
//! its output by lines, of both streams together in the order they came
//! (as `cargo heli log --tail` counts them), each line on its own stream
//! ([`HeadTail`]):
//!
//! - the first [`HEAD`] lines, as they come;
//! - once the run is over, the last [`TAIL`] lines; when lines were cut
//!   between the two, a line on stderr before them says how many, and where
//!   they all are: `cargo-heli: 1234 lines cut (cargo heli log r17 has them
//!   all)`.
//!
//! Quiet mode's line counts the lines: `1314 lines`.

use std::collections::VecDeque;

use super::{LineBuffer, Render, Sink};
use crate::protocol::Channel;

/// Lines shown from the start of the output.
const HEAD: usize = 20;
/// Lines shown from the end of the output.
const TAIL: usize = 60;

/// Renders a command's output as its head and its tail (see the module
/// docs).
pub struct HeadTail {
    /// The start of a line, per stream: stdout, stderr.
    pending: [LineBuffer; 2],
    /// Lines seen so far.
    lines: usize,
    /// The last lines after the head, at most [`TAIL`].
    tail: VecDeque<(Channel, Vec<u8>)>,
}

impl HeadTail {
    pub fn new() -> Self {
        Self {
            pending: [LineBuffer::default(), LineBuffer::default()],
            lines: 0,
            tail: VecDeque::with_capacity(TAIL),
        }
    }

    fn line(&mut self, channel: Channel, line: Vec<u8>, out: &mut Sink) {
        self.lines += 1;
        if self.lines <= HEAD {
            out.write(channel, &line);
            return;
        }
        if self.tail.len() == TAIL {
            self.tail.pop_front();
        }
        self.tail.push_back((channel, line));
    }
}

/// `1 line`, `3 lines`, `1 line cut`.
fn lines(count: usize) -> String {
    if count == 1 {
        "1 line".to_owned()
    } else {
        format!("{count} lines")
    }
}

impl Render for HeadTail {
    fn output(&mut self, channel: Channel, bytes: &[u8], out: &mut Sink) {
        let stream = match channel {
            Channel::Stdout => 0,
            Channel::Stderr => 1,
        };
        for line in self.pending[stream].push(bytes) {
            self.line(channel, line, out);
        }
    }

    fn finish(&mut self, run: &str, out: &mut Sink) -> Vec<String> {
        // The streams' unended last lines, stdout's first.
        for (stream, channel) in [(0, Channel::Stdout), (1, Channel::Stderr)] {
            if let Some(line) = self.pending[stream].rest() {
                self.line(channel, line, out);
            }
        }
        let cut = self.lines.saturating_sub(HEAD + TAIL);
        if cut > 0 {
            out.line(&format!(
                "cargo-heli: {} cut (cargo heli log {run} has them all)",
                lines(cut)
            ));
        }
        for (channel, line) in self.tail.drain(..) {
            out.write(channel, &line);
        }
        vec![lines(self.lines)]
    }
}
