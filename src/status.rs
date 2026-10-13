//! `cargo heli status`: the document `--json` prints, and its text form.
//!
//! The JSON document is a contract for tools: fields are only added, and a
//! change to the meaning of one raises [`SCHEMA`]. The same document travels
//! from the daemon to the client. Times are whole seconds since 1970 (UTC).
//!
//! ```json
//! {"schema":1,
//!  "daemon":{"pid":4242,"protocol":1,"started_at":1791910980},
//!  "runs":[{"id":"r17","worktree":"D:\\work\\agent-a64b","argv":["cargo","build"],
//!           "phase":"running","started_at":1791911112}]}
//! ```
//!
//! `daemon` is `null` when no daemon answers.

use serde::{Deserialize, Serialize};

/// The version of the `--json` schema.
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub schema: u32,
    pub daemon: Option<Daemon>,
    pub runs: Vec<Run>,
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
    pub phase: Phase,
    pub started_at: u64,
}

/// Where a run is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Executing, as one phase (no build and test phases of its own).
    Running,
}

impl Phase {
    /// The name `--json` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
        }
    }
}

impl Status {
    /// The status when no daemon answers.
    pub fn without_daemon() -> Self {
        Self {
            schema: SCHEMA,
            daemon: None,
            runs: Vec::new(),
        }
    }

    /// The text form, one line per daemon and run, with ages as of `now`.
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
        if self.runs.is_empty() {
            text.push_str("runs    none\n");
        }
        for (index, run) in self.runs.iter().enumerate() {
            let label = if index == 0 { "runs" } else { "" };
            text.push_str(&format!(
                "{label:<6}  {}  {}  {}  {}  {}s\n",
                run.id,
                run.worktree,
                run.argv.join(" "),
                run.phase.name(),
                age(run.started_at)
            ));
        }
        text
    }
}
