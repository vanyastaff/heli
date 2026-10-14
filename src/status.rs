//! `cargo heli status`: the document `--json` prints, and its text form.
//!
//! The JSON document is a contract for tools: fields are only added, and a
//! change to the meaning of one raises [`SCHEMA`]. The same document travels
//! from the daemon to the client. Times are whole seconds since 1970 (UTC).
//!
//! ```json
//! {"schema":1,
//!  "daemon":{"pid":4242,"protocol":1,"started_at":1791910980},
//!  "pool":{"slots":32,"free":20,"withheld":0,"in_use":12},
//!  "memory":{"total":68719476736,"available":44291850240,
//!            "commit_available":42521726976,
//!            "low_watermark":6871947673,"high_watermark":11166914969},
//!  "runs":[{"id":"r17","worktree":"D:\\work\\agent-a64b","argv":["cargo","build"],
//!           "class":"compile","phase":"running","started_at":1791911112,
//!           "waiting_since":null,"waiting_for":[],"slots":6,"share":8.0},
//!          {"id":"r18","worktree":"D:\\work\\agent-a64b","argv":["cargo","fmt"],
//!           "class":"mutate","phase":"running","started_at":1791911120,
//!           "waiting_since":1791911120,"waiting_for":["r17"],"slots":0,"share":4.0}]}
//! ```
//!
//! `daemon`, `pool` and `memory` are `null` when no daemon answers; `memory`
//! is also `null` while the daemon cannot read the machine's memory. A run
//! waiting for its worktree's lock has `waiting_since` and, in
//! `waiting_for`, the runs it waits for; otherwise they are `null` and
//! empty. `class` is `null` only from a daemon older than run classes. A
//! run's `slots` are the host pool's slots it uses, and `share` its share of
//! them (`crate::pool::broker`).

use serde::{Deserialize, Serialize};

use crate::class::Class;

/// The version of the `--json` schema.
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub schema: u32,
    pub daemon: Option<Daemon>,
    /// Absent from a daemon older than the pool.
    #[serde(default)]
    pub pool: Option<Pool>,
    /// Absent from a daemon older than the memory governor.
    #[serde(default)]
    pub memory: Option<Memory>,
    pub runs: Vec<Run>,
}

/// The host pool's slots (`crate::pool`), counted as the status was taken.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    /// N: the jobs the pool admits beyond each process's own first one.
    pub slots: u32,
    /// Slots that no process holds.
    pub free: u32,
    /// Slots the daemon holds back while memory is low.
    pub withheld: u32,
    /// Slots that builds hold: `slots - free - withheld`, never below 0.
    pub in_use: u32,
}

/// The memory the governor saw last, and its watermarks, in bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
    /// Available commit, on Windows only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_available: Option<u64>,
    /// Below it, the governor withholds slots.
    pub low_watermark: u64,
    /// Above it, the governor returns them.
    pub high_watermark: u64,
}

/// The daemon that answered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Daemon {
    pub pid: u32,
    /// The protocol version it speaks.
    pub protocol: u32,
    pub started_at: u64,
}

/// A run in flight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    /// `r<number>`, unique on this machine across daemon restarts.
    pub id: String,
    /// The worktree the run works in (`crate::worktree`).
    pub worktree: String,
    /// The command, program first.
    pub argv: Vec<String>,
    #[serde(default)]
    pub class: Option<Class>,
    pub phase: Phase,
    pub started_at: u64,
    /// Since when the run waits for its worktree's lock, if it does.
    #[serde(default)]
    pub waiting_since: Option<u64>,
    /// The runs it waits for, by id.
    #[serde(default)]
    pub waiting_for: Vec<String>,
    /// The slots it uses: lent to its run pool and not free there, and its
    /// test slots.
    #[serde(default)]
    pub slots: u32,
    /// Its share of the host pool's capacity, which it may exceed while no
    /// other run waits.
    #[serde(default)]
    pub share: f64,
}

/// Where a run is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Executing, as one phase (no build and test phases of its own).
    Running,
    /// A test run building its tests.
    Building,
    /// A test run waiting for its test slots, then running its tests.
    Testing,
}

impl Phase {
    /// The name `--json` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Building => "building",
            Self::Testing => "testing",
        }
    }
}

impl Status {
    /// The status when no daemon answers.
    pub fn without_daemon() -> Self {
        Self {
            schema: SCHEMA,
            daemon: None,
            pool: None,
            memory: None,
            runs: Vec::new(),
        }
    }

    /// The text form, one line for the daemon, its pool and each run, with
    /// ages as of `now`.
    pub fn to_text(&self, now: u64) -> String {
        let age = |since: u64| now.saturating_sub(since);
        let mut text = match &self.daemon {
            None => "daemon  not running\n".to_owned(),
            Some(daemon) => format!(
                "daemon  pid {}, protocol {}, up {}s\n",
                daemon.pid,
                daemon.protocol,
                age(daemon.started_at)
            ),
        };
        if let Some(pool) = &self.pool {
            text.push_str(&format!(
                "pool    {} slots: {} in use, {} free, {} withheld\n",
                pool.slots, pool.in_use, pool.free, pool.withheld
            ));
        }
        if let Some(memory) = &self.memory {
            text.push_str(&format!(
                "memory  {} of {} available",
                gigabytes(memory.available),
                gigabytes(memory.total)
            ));
            if let Some(commit) = memory.commit_available {
                text.push_str(&format!(", commit {} available", gigabytes(commit)));
            }
            text.push_str(&format!(
                "; slots withheld below {}, returned above {}\n",
                gigabytes(memory.low_watermark),
                gigabytes(memory.high_watermark)
            ));
        }
        if self.runs.is_empty() {
            text.push_str("runs    none\n");
        }
        for (index, run) in self.runs.iter().enumerate() {
            let label = if index == 0 { "runs" } else { "" };
            let state = if run.waiting_for.is_empty() {
                run.phase.name().to_owned()
            } else {
                format!("waiting for {}", run.waiting_for.join(", "))
            };
            text.push_str(&format!(
                "{label:<6}  {}  {}  {}  {}  {state}  {} slot{}, share {}  {}s\n",
                run.id,
                run.worktree,
                run.argv.join(" "),
                run.class.map_or("-", Class::name),
                run.slots,
                if run.slots == 1 { "" } else { "s" },
                slots(run.share),
                age(run.started_at)
            ));
        }
        text
    }
}

/// A number of slots for people: whole, or with one decimal.
pub fn slots(slots: f64) -> String {
    if slots.fract() == 0.0 {
        format!("{slots:.0}")
    } else {
        format!("{slots:.1}")
    }
}

/// `bytes` in gigabytes of 2^30 bytes, to a tenth.
fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1u32 << 30))
}
