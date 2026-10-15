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

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::ipc::Stream;
use crate::protocol::{self, Channel, Reply};
use crate::run_id::RunId;
use crate::runlog::{self, Events};
use crate::status;

/// How often a follower with nothing to send checks that its client is
/// still there.
const TICK: Duration = Duration::from_millis(200);

/// Once the run ends, a stalled follower cannot pin a retiring daemon.
/// Its complete output remains in the record for a later `log` request.
const DRAIN_WAIT: Duration = Duration::from_secs(1);

/// The output of one run, as it grows.
pub struct Live {
    /// The run log's directory.
    dir: PathBuf,
    run: RunId,
    journal: Mutex<Journal>,
    changed: Condvar,
}

/// What a follower can send so far, in order.
#[derive(Default)]
struct Journal {
    items: Vec<Item>,
    /// The reply that ends the run, once all of the above is complete.
    end: Option<Arc<Reply>>,
    ended_at: Option<Instant>,
}

#[derive(Clone)]
enum Item {
    /// The next this many events of the record.
    Recorded(u64),
    /// Output that is not in the record.
    Kept(Channel, Vec<u8>),
    /// The runs the run waits for, before it starts.
    Waiting(Vec<status::Run>),
    /// The run's command has started.
    Spawned,
}

impl Item {
    /// How many things a follower sends for it.
    fn len(&self) -> u64 {
        match self {
            Self::Recorded(count) => *count,
            Self::Kept(..) | Self::Waiting(_) | Self::Spawned => 1,
        }
    }
}

impl Live {
    /// The output of run `run`, recorded in the run log `dir`.
    pub fn new(dir: PathBuf, run: RunId) -> Self {
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

    /// Adds that the run's command has started, once: a later phase's start
    /// adds nothing.
    pub fn spawned(&self) {
        let mut journal = self.journal();
        if !journal
            .items
            .iter()
            .any(|item| matches!(item, Item::Spawned))
        {
            journal.items.push(Item::Spawned);
            self.changed.notify_all();
        }
    }

    /// Ends the run for its clients with `reply`, after all of the above.
    pub fn end(&self, reply: Reply) {
        let mut journal = self.journal();
        journal.end = Some(Arc::new(reply));
        journal.ended_at = Some(Instant::now());
        drop(journal);
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

/// What a client following a run reads besides its output and its end.
#[derive(Debug, Clone, Copy)]
pub struct Reads {
    /// What the run waits for ([`Reply::Waiting`]): a client that knows run
    /// classes.
    pub waiting: bool,
    /// That the run's command started ([`Reply::Spawned`]).
    pub spawned: bool,
}

/// Sends `client` the run's output from its first byte, then the rest as
/// it comes, then the run's end; returns early when the client goes away,
/// or the record cannot be read back. A client is also told what the run
/// waits for, and when its command started, as far as it `reads` them.
pub fn follow(
    live: &Live,
    client: &mut Stream,
    reads: Reads,
    retiring: &dyn Fn() -> bool,
) -> io::Result<()> {
    let mut client = Follower {
        stream: client,
        live,
        retiring,
    };
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
                        record = runlog::open(&live.dir, live.run).ok();
                    }
                    for _ in 0..count {
                        let event = match record.as_mut().and_then(Iterator::next) {
                            Some(Ok(event)) if event.index == read => event,
                            _ => {
                                return Err(io::Error::other(
                                    "run record does not contain the published event",
                                ));
                            }
                        };
                        protocol::write_output(&mut client, event.channel, &event.bytes)?;
                        read += 1;
                        sent += 1;
                    }
                }
                Item::Kept(channel, bytes) => {
                    protocol::write_output(&mut client, channel, &bytes)?;
                    sent += 1;
                }
                Item::Waiting(runs) => {
                    if reads.waiting {
                        protocol::write_message(&mut client, &Reply::Waiting { runs })?;
                    }
                    sent += 1;
                }
                Item::Spawned => {
                    if reads.spawned {
                        protocol::write_message(&mut client, &Reply::Spawned)?;
                    }
                    sent += 1;
                }
            }
        }
        if let Some(end) = end {
            return protocol::write_message(&mut client, end.as_ref());
        }
        if sent == before && !client.stream.connected() {
            return Ok(());
        }
    }
}

/// Retry partial writes while the run is live, keeping their exact offset.
/// A timeout never restarts a frame, which would corrupt its byte stream.
struct Follower<'a> {
    stream: &'a mut Stream,
    live: &'a Live,
    retiring: &'a dyn Fn() -> bool,
}

impl Write for Follower<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.stream.set_deadline(Some(Instant::now() + TICK))?;
            match self.stream.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    if (self.retiring)()
                        && self
                            .live
                            .journal()
                            .ended_at
                            .is_some_and(|at| at.elapsed() >= DRAIN_WAIT)
                    {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
