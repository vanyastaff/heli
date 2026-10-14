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
//! [`TICK`] even while the run is silent. A follower holds a pin on the
//! record (`RunLog::pin`), so the record is not pruned under it.
//!
//! Besides the record, clients get what is not in it, in its place among
//! the output: output that could not be recorded (the record could not be
//! created, or a write to it failed), kept here in memory from the first
//! event lost; heli's notes about the run; and, before it starts, the runs
//! it waits for. The end is the reply that ends the run for its clients:
//! how it exited, or that it never ran.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::ipc::Stream;
use crate::protocol::{self, Channel, Reply};
use crate::runlog::{self, Events};
use crate::status;

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

/// What a follower can send so far, in order.
#[derive(Default)]
struct Journal {
    items: Vec<Item>,
    /// The reply that ends the run, once all of the above is complete.
    end: Option<Arc<Reply>>,
}

#[derive(Clone)]
enum Item {
    /// The next this many events of the record.
    Recorded(u64),
    /// Output that is not in the record.
    Kept(Channel, Vec<u8>),
    /// The runs the run waits for, before it starts.
    Waiting(Vec<status::Run>),
}

impl Item {
    /// How many things a follower sends for it.
    fn len(&self) -> u64 {
        match self {
            Self::Recorded(count) => *count,
            Self::Kept(..) | Self::Waiting(_) => 1,
        }
    }
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

    /// Adds what the run wrote to `channel`; `recorded` says whether the
    /// record holds it (once recording has failed, it never resumes).
    pub fn append(&self, channel: Channel, bytes: Vec<u8>, recorded: bool) {
        let mut journal = self.journal();
        match journal.items.last_mut() {
            Some(Item::Recorded(count)) if recorded => *count += 1,
            _ if recorded => journal.items.push(Item::Recorded(1)),
            _ => journal.items.push(Item::Kept(channel, bytes)),
        }
        self.changed.notify_all();
    }

    /// Adds heli's note about the run, a line on its stderr.
    pub fn note(&self, note: &str) {
        let line = format!("cargo-heli: note: {note}\n");
        self.push(Item::Kept(Channel::Stderr, line.into_bytes()));
    }

    /// Adds the runs the run waits for before it starts.
    pub fn waiting(&self, runs: Vec<status::Run>) {
        self.push(Item::Waiting(runs));
    }

    /// Ends the run for its clients with `reply`, after all of the above.
    pub fn end(&self, reply: Reply) {
        self.journal().end = Some(Arc::new(reply));
        self.changed.notify_all();
    }

    fn push(&self, item: Item) {
        self.journal().items.push(item);
        self.changed.notify_all();
    }

    /// Waits (one [`TICK`] at most) until there is more than the first
    /// `sent` things to send, or the end; returns what there is beyond them
    /// then, and the end.
    fn beyond(&self, sent: u64) -> (Vec<Item>, Option<Arc<Reply>>) {
        let mut journal = self.journal();
        let total: u64 = journal.items.iter().map(Item::len).sum();
        if total <= sent && journal.end.is_none() {
            journal = self
                .changed
                .wait_timeout(journal, TICK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let mut skip = sent;
        let mut items = Vec::new();
        for item in &journal.items {
            let len = item.len();
            if skip >= len {
                skip -= len;
                continue;
            }
            items.push(match item {
                Item::Recorded(count) => Item::Recorded(count - skip),
                other => other.clone(),
            });
            skip = 0;
        }
        (items, journal.end.clone())
    }

    fn journal(&self) -> MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Sends `client` the run's output from its first byte, then the rest as
/// it comes, then the run's end; returns early when the client goes away,
/// or the record cannot be read back. A client that `can_wait` (one that
/// knows run classes) is also told what the run waits for.
pub fn follow(live: &Live, client: &mut Stream, can_wait: bool) {
    let mut record: Option<Events> = None;
    // Events read from the record, and things sent (or skipped).
    let mut read = 0;
    let mut sent = 0;
    loop {
        let (items, end) = live.beyond(sent);
        let before = sent;
        for item in items {
            match item {
                Item::Recorded(count) => {
                    if record.is_none() {
                        record = runlog::open(&live.dir, &live.run).ok();
                    }
                    for _ in 0..count {
                        let event = match record.as_mut().and_then(Iterator::next) {
                            Some(Ok(event)) if event.index == read => event,
                            _ => return,
                        };
                        if protocol::write_output(client, event.channel, &event.bytes).is_err() {
                            return;
                        }
                        read += 1;
                        sent += 1;
                    }
                }
                Item::Kept(channel, bytes) => {
                    if protocol::write_output(client, channel, &bytes).is_err() {
                        return;
                    }
                    sent += 1;
                }
                Item::Waiting(runs) => {
                    if can_wait
                        && protocol::write_message(client, &Reply::Waiting { runs }).is_err()
                    {
                        return;
                    }
                    sent += 1;
                }
            }
        }
        if let Some(end) = end {
            let _ = protocol::write_message(client, end.as_ref());
            return;
        }
        if sent == before && !client.connected() {
            return;
        }
    }
}
