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
//!  "runs":[{"id":"r17","worktree":"D:\\work\\agent-a64b","argv":["cargo","build"],
//!           "class":"compile","phase":"running","started_at":1791911112,
//!           "waiting_since":null,"waiting_for":[]},
//!          {"id":"r18","worktree":"D:\\work\\agent-a64b","argv":["cargo","fmt"],
//!           "class":"mutate","phase":"running","started_at":1791911120,
//!           "waiting_since":1791911120,"waiting_for":["r17"]}],
//!  "disk":{"stores":[{"path":"D:\.heli\build","budget":null,"used":1000,
//!                     "by_category":{"incremental":600,"deps":300,"build_scripts":50,
//!                                    "fingerprints":10,"tool_caches":0,"other":40}}],
//!          "targets":[{"path":"D:\work\agent-a64b\target","used":120,
//!                      "by_category":{"incremental":0,"deps":0,"build_scripts":0,
//!                                     "fingerprints":0,"tool_caches":20,"other":100}}]}}
//! ```
//!
//! `daemon` and `pool` are `null` when no daemon answers. A run waiting for
//! its worktree's lock has `waiting_since` and, in `waiting_for`, the runs
//! it waits for; otherwise they are `null` and empty. `class` is `null` only
//! from a daemon older than run classes.
//!
//! `disk` is the build store as the daemon last counted it
//! (`crate::daemon::accounts`): each store's bytes in use, by category
//! (`crate::store::sizes`), and its `budget` (`null`: no budget yet); and
//! each registered `target/` directory's. It is `null` when no daemon
//! answers, or one older than the build store.

use serde::{Deserialize, Serialize};

use crate::class::Class;
use crate::store::sizes::{self, Sizes};

/// The version of the `--json` schema.
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub schema: u32,
    pub daemon: Option<Daemon>,
    /// Absent from a daemon older than the pool.
    #[serde(default)]
    pub pool: Option<Pool>,
    pub runs: Vec<Run>,
    /// Absent from a daemon older than the build store.
    #[serde(default)]
    pub disk: Option<Disk>,
}

/// The disk the build store takes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Disk {
    /// One entry per store.
    pub stores: Vec<StoreUse>,
    /// One entry per registered `target/` directory.
    #[serde(default)]
    pub targets: Vec<TargetUse>,
}

/// A store's build directories, added up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreUse {
    pub path: String,
    /// The bytes the store may take; `None` until budgets exist.
    pub budget: Option<u64>,
    pub used: u64,
    pub by_category: Sizes,
}

/// A registered `target/` directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetUse {
    pub path: String,
    pub used: u64,
    pub by_category: Sizes,
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
            pool: None,
            runs: Vec::new(),
            disk: None,
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
                "{label:<6}  {}  {}  {}  {}  {state}  {}s\n",
                run.id,
                run.worktree,
                run.argv.join(" "),
                run.class.map_or("-", Class::name),
                age(run.started_at)
            ));
        }
        if let Some(disk) = &self.disk {
            let mut label = "disk";
            for store in &disk.stores {
                text.push_str(&format!(
                    "{label:<6}  {}  {}
",
                    store.path,
                    used(store.used, &store.by_category)
                ));
                label = "";
            }
            for target in &disk.targets {
                text.push_str(&format!(
                    "{label:<6}  target {}  {}
",
                    target.path,
                    used(target.used, &target.by_category)
                ));
                label = "";
            }
        }
        text
    }
}

/// `used` bytes, then the categories that hold any:
/// `1.2 GiB (incremental 800.0 MiB, deps 400.0 MiB)`.
fn used(used: u64, by_category: &Sizes) -> String {
    let parts: Vec<String> = [
        ("incremental", by_category.incremental),
        ("deps", by_category.deps),
        ("build scripts", by_category.build_scripts),
        ("fingerprints", by_category.fingerprints),
        ("tool caches", by_category.tool_caches),
        ("other", by_category.other),
    ]
    .into_iter()
    .filter(|(_, bytes)| *bytes > 0)
    .map(|(name, bytes)| format!("{name} {}", sizes::human(bytes)))
    .collect();
    if parts.is_empty() {
        sizes::human(used)
    } else {
        format!("{} ({})", sizes::human(used), parts.join(", "))
    }
}
