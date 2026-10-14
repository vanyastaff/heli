//! A run's output as its clients follow it: from its first byte, as it
//! comes, then how the run ended.
//!
//! The run's output goes into its record in the run log (`crate::runlog`),
//! and from there to clients: the run itself never waits for a client.
//! Each client following the run (the call that started it, and any that
//! attached to it later) has a thread of its own, [`follow`], which reads
//! the record from the start, sends it on, and waits for more. A client
//! that reads slowly or not at all falls behind and holds up nobody; one
//! that goes away ends only its own thread, which notices within
//! [`TICK`] even while the run is silent.
//!
//! Output that could not be recorded (the record could not be created, or
//! a write to it failed) is kept here in memory instead, from the first
//! event lost, so clients still get all of it.

use std::path::PathBuf;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::ipc::Stream;
use crate::protocol::{self, Channel, Reply};
use crate::runlog::{self, Events};
use crate::status::Exit;

/// How often a follower with nothing to send checks that its client is
/// still there.
const TICK: Duration = Duration::from_millis(200);

/// The output of one run, as it grows.
pub struct Live {
    /// The run log's directory.
    dir: PathBuf,
    /// The run, `r<number>`.
    run: String,
    journal: Mutex<Journal>,
    changed: Condvar,
}

/// What a follower can send so far.
#[derive(Default)]
struct Journal {
    /// The events in the record, from the first.
    recorded: u64,
    /// The events after those, which could not be recorded.
    kept: Vec<(Channel, Vec<u8>)>,
    /// How the run ended, once it has, and all its output is above.
    end: Option<Exit>,
}

impl Live {
    /// The output of run `run`, recorded in the run log `dir`.
    pub fn new(dir: PathBuf, run: String) -> Self {
        Self {
            dir,
            run,
            journal: Mutex::default(),
            changed: Condvar::new(),
        }
    }

    /// The record, from its start, for a new follower: `None` when it
    /// cannot be opened (then nothing has been recorded either). Open it
    /// while the run is in flight, when it cannot be pruned.
    pub fn open(&self) -> Option<Events> {
        runlog::open(&self.dir, &self.run).ok()
    }

    /// Adds what the run wrote to `channel`; `recorded` says whether the
    /// record holds it (once recording has failed, it never resumes).
    pub fn append(&self, channel: Channel, bytes: Vec<u8>, recorded: bool) {
        let mut journal = self.journal();
        if recorded {
            journal.recorded += 1;
        } else {
            journal.kept.push((channel, bytes));
        }
        self.changed.notify_all();
    }

    /// Marks the run as ended, after all of its output.
    pub fn end(&self, exit: Exit) {
        self.journal().end = Some(exit);
        self.changed.notify_all();
    }

    /// Waits (one [`TICK`] at most) until there is more than the first
    /// `sent` events, or the end; returns the journal as it is then, with
    /// only the events kept from `sent` on.
    fn beyond(&self, sent: u64) -> Journal {
        let mut journal = self.journal();
        let total = journal.recorded + journal.kept.len() as u64;
        if total <= sent && journal.end.is_none() {
            journal = self
                .changed
                .wait_timeout(journal, TICK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let skip = usize::try_from(sent.saturating_sub(journal.recorded)).unwrap_or(usize::MAX);
        Journal {
            recorded: journal.recorded,
            kept: journal.kept.get(skip..).unwrap_or_default().to_vec(),
            end: journal.end,
        }
    }

    fn journal(&self) -> MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Sends `client` the run's output from its first byte (`record`, opened
/// with [`Live::open`]), then the rest as it comes, then how the run ended;
/// returns early when the client goes away, or the record cannot be read
/// back.
pub fn follow(live: &Live, mut record: Option<Events>, client: &mut Stream) {
    let mut sent = 0;
    loop {
        let Journal {
            recorded,
            kept,
            end,
        } = live.beyond(sent);
        let before = sent;
        while sent < recorded {
            let event = match record.as_mut().and_then(Iterator::next) {
                Some(Ok(event)) if event.index == sent => event,
                _ => return,
            };
            if protocol::write_output(client, event.channel, &event.bytes).is_err() {
                return;
            }
            sent += 1;
        }
        for (channel, bytes) in &kept {
            if protocol::write_output(client, *channel, bytes).is_err() {
                return;
            }
            sent += 1;
        }
        if let Some(exit) = end {
            let _ = protocol::write_message(client, &Reply::exited(exit));
            return;
        }
        if sent == before && !client.connected() {
            return;
        }
    }
}
