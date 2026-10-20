//! `cargo heli status`: the document `--json` prints, and its text form.
//!
//! The JSON document is a contract for tools: fields are only added, and a
//! change to the meaning of one raises [`SCHEMA`]. The same document travels
//! from the daemon to the client. Times are whole seconds since 1970 (UTC).
//!
//! ```json
//! {"schema":1,
//!  "daemon":{"pid":4242,"version":"0.0.3",
//!            "executable":"C:\\Users\\me\\.cargo\\bin\\cargo-heli.exe",
//!            "protocol":1,"started_at":1791910980,"session":1,
//!            "window_station":"WinSta0","desktop":"Default","interactive":true},
//!  "pool":{"slots":32,"free":20,"withheld":0,"in_use":12},
//!  "memory":{"total":68719476736,"available":44291850240,
//!            "commit_available":42521726976,
//!            "low_watermark":6871947673,"high_watermark":11166914969},
//!  "runs":[{"id":"r17","worktree":"D:\\work\\agent-a64b","argv":["cargo","build"],
//!           "class":"compile","priority":"interactive","phase":"running",
//!           "started_at":1791911112,
//!           "waiting_since":null,"waiting_for":[],"slots":6,"share":8.0,
//!           "attached_clients":1,"ended_at":null,"exit":null},
//!          {"id":"r18","worktree":"D:\\work\\agent-a64b","argv":["cargo","fmt"],
//!           "class":"mutate","priority":"normal","phase":"running",
//!           "started_at":1791911120,
//!           "waiting_since":1791911120,"waiting_for":["r17"],"slots":0,"share":4.0,
//!           "attached_clients":2,"ended_at":null,"exit":null},
//!          {"id":"r16","worktree":"D:\\work\\agent-f1c0","argv":["cargo","test"],
//!           "class":"test","priority":"background","phase":"done",
//!           "started_at":1791910990,
//!           "waiting_since":null,"waiting_for":[],"slots":0,"share":0.0,
//!           "attached_clients":0,"ended_at":1791911050,
//!           "exit":{"code":101,"signal":null,"cancelled":false}}],
//!  "disk":{"stores":[{"path":"D:\\.heli\\build","budget":1200,"used":1000,
//!                     "by_category":{"incremental":600,"deps":300,"build_scripts":50,
//!                                    "fingerprints":10,"tool_caches":0,"other":40}}],
//!          "targets":[{"path":"D:\\work\\agent-a64b\\target","used":120,
//!                      "by_category":{"incremental":0,"deps":0,"build_scripts":0,
//!                                     "fingerprints":0,"tool_caches":20,"other":100},
//!                      "superseded":0}],
//!          "scratch":[]},
//!  "last_gc":{"at":1791911000,
//!             "evicted":[{"path":"D:\\.heli\\build\\28\\4ec2fa37745150\\debug\\incremental\\flui_view-0x1yz",
//!                         "tier":1,"bytes":300}]}}
//! ```
//!
//! `daemon.version` is the daemon's cargo-heli version and `executable` its
//! program, both `null` from a daemon older than them (0.0.2 and before);
//! the text form flags a daemon older than the client.
//! `daemon`, `pool` and `memory` are `null` when no daemon answers; `memory`
//! is also `null` while the daemon cannot read the machine's memory. A run
//! waiting for its worktree's lock has `waiting_since` and, in
//! `waiting_for`, the runs it waits for; otherwise they are `null` and
//! empty. `class` is `null` only from a daemon older than run classes.
//! `profile` says where the class came from (`crate::profiles::Source::
//! name`), `null` from a client or daemon older than profiles.
//! `rerun_of` names the run a rerun reruns (`cargo heli rerun`); it is
//! `null` for any other run, and absent from a daemon older than reruns. A
//! run's `priority` (`crate::priority`) is `interactive`, `normal` or
//! `background`; `normal` from a daemon older than priorities. A
//! run's `slots` are the host pool's slots it uses, and `share` its share of
//! them (`crate::pool::broker`), weighted by its priority. `runs` lists the
//! runs in flight, and the last runs that ended while no client followed
//! them (`daemon::registry`), so whoever started them can still learn how
//! they ended.
//!
//! `disk` is the build store as the daemon last counted it
//! (`crate::daemon::accounts`): each store's bytes in use, by category
//! (`crate::store::sizes`), and the `budget` all of them and the registered
//! `target/` directories share (`null`: no budget); and each registered
//! `target/` directory's, with `superseded`: the bytes of its superseded
//! intermediates (`crate::store::superseded`), which `by_category` counts
//! too. It is `null` when no daemon answers, or one older
//! than the build store. `last_gc` is the daemon's last eviction pass that
//! evicted anything (docs/build-store.md#eviction): when, and each victim's
//! `path`, `tier` and `bytes`; `null` before the first.

use serde::{Deserialize, Serialize};

use crate::class::Class;
use crate::priority::Priority;
use crate::store::sizes::{self, Sizes};

/// Whether version `a` (`major.minor.patch`, perhaps with a suffix) is
/// older than `b`; a version that does not read as numbers is not.
pub fn older(a: &str, b: &str) -> bool {
    let numbers = |version: &str| -> Option<Vec<u64>> {
        version
            .split(['-', '+'])
            .next()?
            .split('.')
            .map(|part| part.parse().ok())
            .collect()
    };
    matches!((numbers(a), numbers(b)), (Some(a), Some(b)) if a < b)
}

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
    /// Absent from a daemon older than the build store.
    #[serde(default)]
    pub disk: Option<Disk>,
    /// The daemon's last eviction pass that evicted anything; absent from a
    /// daemon older than eviction.
    #[serde(default)]
    pub last_gc: Option<LastGc>,
    /// The budget the daemon's eviction passes cannot meet, while the busy
    /// directories alone hold more than its target (#122); absent from an
    /// older daemon, and while the budget can be met.
    #[serde(default)]
    pub unmet_budget: Option<UnmetBudget>,
}

/// A budget the daemon's eviction passes cannot meet
/// (`crate::store::evict::UnmetBudget`): the busy directories alone hold
/// more than 85 % of it, so live directories are kept whole.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UnmetBudget {
    /// B, in bytes.
    pub budget: u64,
    /// What every directory takes, in bytes.
    pub used: u64,
    /// The busy directories, and what they take, in bytes.
    pub busy_dirs: usize,
    pub busy_bytes: u64,
    /// Since when the budget cannot be met, in seconds since 1970.
    pub since: u64,
}

impl UnmetBudget {
    /// `budget 100.0 GiB cannot be met: 18 busy directories hold 200.0 GiB,
    /// more than 85 % of it`.
    pub fn describe(&self) -> String {
        self.figures().describe()
    }

    /// What holds the budget: `18 busy directories hold 200.0 GiB, more
    /// than 85 % of it`.
    pub fn holders(&self) -> String {
        self.figures().holders()
    }

    fn figures(&self) -> crate::store::evict::UnmetBudget {
        crate::store::evict::UnmetBudget {
            budget: self.budget,
            used: self.used,
            busy_dirs: self.busy_dirs,
            busy_bytes: self.busy_bytes,
        }
    }
}

/// An eviction pass of the daemon's (`crate::store::evict`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastGc {
    pub at: u64,
    pub evicted: Vec<Evicted>,
}

/// A victim the daemon evicted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evicted {
    pub path: String,
    /// Its rung of the ladder, 1 to 5.
    pub tier: u8,
    pub bytes: u64,
}

/// The disk the build store takes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Disk {
    /// One entry per store.
    pub stores: Vec<StoreUse>,
    /// One entry per registered `target/` directory.
    #[serde(default)]
    pub targets: Vec<TargetUse>,
    /// One entry per scratch copy a cargo tool left behind
    /// (`crate::store::scratch`).
    #[serde(default)]
    pub scratch: Vec<TargetUse>,
}

/// A store's build directories, added up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreUse {
    pub path: String,
    /// B, the budget every store and registered `target/` directory share
    /// (`[store] budget_mb`), in bytes; `None` without one.
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
    /// Bytes in its superseded intermediates, also in `by_category`; 0 from
    /// a daemon older than this field.
    #[serde(default)]
    pub superseded: u64,
    /// Seen for more than one workspace: the build store leaves it alone.
    #[serde(default)]
    pub shared: bool,
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
    /// Its cargo-heli version; absent from a daemon older than this field.
    #[serde(default)]
    pub version: Option<String>,
    /// Its program; absent from a daemon older than this field.
    #[serde(default)]
    pub executable: Option<String>,
    /// The protocol version it speaks.
    pub protocol: u32,
    pub started_at: u64,
    /// Its session: on Windows the logon session (Terminal Services
    /// session id), elsewhere its session (`getsid`). Absent from a daemon
    /// older than this field, and `null` when it cannot be read.
    #[serde(default)]
    pub session: Option<u32>,
    /// Its window station, on Windows (`WinSta0` is the interactive one).
    #[serde(default)]
    pub window_station: Option<String>,
    /// Its desktop, on Windows (`Default` is the user's).
    #[serde(default)]
    pub desktop: Option<String>,
    /// Whether it runs on the user's interactive desktop: on Windows the
    /// window station `WinSta0` of a session other than 0. `null` where
    /// heli cannot tell.
    #[serde(default)]
    pub interactive: Option<bool>,
}

/// A run in flight, or one that ended while no client followed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    /// `r<number>`, unique for the life of the state directory.
    pub id: String,
    /// The worktree the run works in (`crate::worktree`).
    pub worktree: String,
    /// The command, program first.
    pub argv: Vec<String>,
    /// `None` from a daemon older than run classes, and for a class this
    /// build does not know (a newer daemon's).
    #[serde(default, deserialize_with = "known_class")]
    pub class: Option<Class>,
    /// Where its class came from (`crate::profiles::Source::name`); `None`
    /// from a client or daemon older than profiles.
    #[serde(default)]
    pub profile: Option<String>,
    /// Its weight in the shares; normal from a daemon older than
    /// priorities, and for a priority this build does not know.
    #[serde(default, deserialize_with = "crate::priority::known")]
    pub priority: Priority,
    /// The run it reruns (`cargo heli rerun`); `None` for a run that is no
    /// rerun, and from a daemon older than reruns.
    #[serde(default)]
    pub rerun_of: Option<String>,
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
    /// The clients following the run now: the call that started it and
    /// those that attached to it.
    #[serde(default)]
    pub attached_clients: u32,
    /// When it ended; `null` while it runs.
    #[serde(default)]
    pub ended_at: Option<u64>,
    /// How it ended; `null` while it runs.
    #[serde(default)]
    pub exit: Option<Exit>,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    /// The code its clients exit with: the command's own, or on Unix 128
    /// plus the signal that killed it.
    pub code: i32,
    /// On Unix, the signal that killed it.
    pub signal: Option<i32>,
    /// Whether `cargo heli cancel` ended it.
    pub cancelled: bool,
}

impl Exit {
    /// `exit <code>` or `signal <n>`, after `cancelled, ` for a cancelled
    /// run.
    pub fn describe(&self) -> String {
        let ending = match self.signal {
            Some(signal) => format!("signal {signal}"),
            None => format!("exit {}", self.code),
        };
        if self.cancelled {
            format!("cancelled, {ending}")
        } else {
            ending
        }
    }
}

/// A run's class as `status` reads it: `None` for a class this build does
/// not know, which a newer daemon may send.
fn known_class<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Class>, D::Error> {
    let class = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(class.and_then(|class| serde_json::from_value(class).ok()))
}

/// Where a run is in its lifecycle. A newer daemon may add phases, so a
/// phase this build does not know reads as [`Phase::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Executing, as one phase (no build and test phases of its own).
    Running,
    /// A test run building its tests.
    Building,
    /// A test run waiting for its test slots, then running its tests.
    Testing,
    /// Ended.
    Done,
    /// A phase of a newer daemon's, which this build does not know.
    #[serde(other)]
    Unknown,
}

impl Phase {
    /// The name `--json` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Building => "building",
            Self::Testing => "testing",
            Self::Done => "done",
            Self::Unknown => "unknown",
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
            disk: None,
            last_gc: None,
            unmet_budget: None,
        }
    }

    /// The text form, one line for the daemon, its pool and each run, with
    /// ages as of `now`.
    pub fn to_text(&self, now: u64) -> String {
        let age = |since: u64| now.saturating_sub(since);
        let mut text = match &self.daemon {
            None => "daemon  not running\n".to_owned(),
            Some(daemon) => {
                let mut place = String::new();
                if let Some(session) = daemon.session {
                    place.push_str(&format!(", session {session}"));
                }
                if let (Some(station), Some(desktop)) = (&daemon.window_station, &daemon.desktop) {
                    place.push_str(&format!(", {station}\\{desktop}"));
                }
                match daemon.interactive {
                    Some(true) => place.push_str(" (interactive)"),
                    Some(false) => place.push_str(" (not interactive)"),
                    None => {}
                }
                let client = env!("CARGO_PKG_VERSION");
                let version = daemon.version.as_deref();
                let mut line = format!(
                    "daemon  pid {}, version {}, protocol {}{place}, up {}s\n",
                    daemon.pid,
                    version.unwrap_or("before 0.0.3"),
                    daemon.protocol,
                    age(daemon.started_at)
                );
                if let Some(program) = &daemon.executable {
                    line.push_str(&format!("        {program}\n"));
                }
                if version.is_none_or(|version| older(version, client)) {
                    line.push_str(&format!(
                        "        older than this cargo-heli ({client}): `cargo heli restart` replaces it\n"
                    ));
                }
                line
            }
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
            let mut state = match &run.exit {
                Some(exit) => format!("{}, {}", run.phase.name(), exit.describe()),
                None if run.waiting_for.is_empty() => {
                    format!("{}, {} attached", run.phase.name(), run.attached_clients)
                }
                None => format!(
                    "waiting for {}, {} attached",
                    run.waiting_for.join(", "),
                    run.attached_clients
                ),
            };
            let priority = if run.priority.is_normal() {
                String::new()
            } else {
                format!("{}  ", run.priority)
            };
            if let Some(of) = &run.rerun_of {
                state.push_str(&format!(", rerun of {of}"));
            }
            text.push_str(&format!(
                "{label:<6}  {}  {}  {}  {}  {state}  {priority}{} slot{}, share {}  {}s\n",
                run.id,
                run.worktree,
                run.argv.join(" "),
                class_of(run),
                run.slots,
                if run.slots == 1 { "" } else { "s" },
                slots(run.share),
                age(run.started_at)
            ));
        }
        if let Some(disk) = &self.disk {
            let mut label = "disk";
            for store in &disk.stores {
                text.push_str(&format!(
                    "{label:<6}  {}  {}\n",
                    store.path,
                    used(store.used, &store.by_category)
                ));
                label = "";
            }
            for target in &disk.targets {
                let superseded = if target.superseded > 0 {
                    format!(", {} superseded", sizes::human(target.superseded))
                } else {
                    String::new()
                };
                text.push_str(&format!(
                    "{label:<6}  target {}{}  {}{superseded}\n",
                    target.path,
                    if target.shared {
                        " (shared, left alone)"
                    } else {
                        ""
                    },
                    used(target.used, &target.by_category)
                ));
                label = "";
            }
            let superseded: u64 = disk.targets.iter().map(|target| target.superseded).sum();
            if superseded > 0 {
                text.push_str(&format!("{label:<6}  {}", crate::gc::hint(superseded)));
                label = "";
            }
            for scratch in &disk.scratch {
                text.push_str(&format!(
                    "{label:<6}  scratch {}  {}\n",
                    scratch.path,
                    sizes::human(scratch.used)
                ));
                label = "";
            }
        }
        if let Some(unmet) = &self.unmet_budget {
            text.push_str(&format!(
                "budget  {} cannot be met for {}s: {}; live directories are kept whole\n",
                sizes::human(unmet.budget),
                now.saturating_sub(unmet.since),
                unmet.holders()
            ));
        }
        if let Some(gc) = &self.last_gc {
            let freed = gc.evicted.iter().map(|evicted| evicted.bytes).sum();
            text.push_str(&format!(
                "gc      {}s ago: evicted {}, {}\n",
                age(gc.at),
                gc.evicted.len(),
                sizes::human(freed)
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

/// A run's class as the text form shows it, with the profile that gave it
/// when that is not cargo's own: `verify (user)`, `exec (unknown)`.
fn class_of(run: &Run) -> String {
    let class = run.class.map_or("-", Class::name);
    match run.profile.as_deref() {
        None | Some("cargo" | "exec") => class.to_owned(),
        Some(profile) => format!("{class} ({profile})"),
    }
}

#[cfg(test)]
mod version_tests {
    use super::older;

    #[test]
    fn compares_versions_by_number() {
        assert!(older("0.0.2", "0.0.3"));
        assert!(older("0.0.9", "0.0.10"));
        assert!(older("0.9.0", "1.0.0"));
        assert!(!older("0.0.3", "0.0.3"));
        assert!(!older("0.1.0", "0.0.9"));
        assert!(!older("0.0.3-dev", "0.0.3"));
        assert!(!older("garbled", "0.0.3"));
    }
}

#[cfg(test)]
mod disk_tests {
    use super::{Disk, Status, TargetUse, UnmetBudget};
    use crate::store::sizes::Sizes;

    /// A budget the daemon cannot meet is one line of the text form, with
    /// what holds it and for how long (#122); without one, no line.
    #[test]
    fn an_unmet_budget_is_shown() {
        let status = Status {
            unmet_budget: Some(UnmetBudget {
                budget: 100 << 30,
                used: 386 << 30,
                busy_dirs: 18,
                busy_bytes: 200 << 30,
                since: 1000,
            }),
            ..Status::without_daemon()
        };
        let text = status.to_text(4600);
        assert!(
            text.contains(
                "budget  100.0 GiB cannot be met for 3600s: 18 busy directories hold 200.0 GiB, more than 85 % of it; live directories are kept whole\n"
            ),
            "{text}"
        );
        assert!(!Status::without_daemon().to_text(0).contains("budget"));
        let older: Status = serde_json::from_value(serde_json::json!({
            "schema": super::SCHEMA, "daemon": null, "runs": []
        }))
        .expect("an older status");
        assert!(older.unmet_budget.is_none());
    }

    #[test]
    fn superseded_intermediates_are_shown_with_a_hint() {
        let target = |path: &str, superseded: u64| TargetUse {
            path: path.to_owned(),
            used: 4096,
            by_category: Sizes {
                deps: 4096,
                ..Sizes::default()
            },
            superseded,
            shared: false,
        };
        let status = Status {
            disk: Some(Disk {
                stores: Vec::new(),
                targets: vec![target("/a/target", 3072), target("/b/target", 0)],
                scratch: Vec::new(),
            }),
            ..Status::without_daemon()
        };
        let text = status.to_text(0);
        assert!(
            text.contains("target /a/target  4.0 KiB (deps 4.0 KiB), 3.0 KiB superseded\n"),
            "{text}"
        );
        assert!(
            text.contains("target /b/target  4.0 KiB (deps 4.0 KiB)\n"),
            "{text}"
        );
        assert!(
            text.contains("3.0 KiB superseded by the build store: cargo heli gc --superseded\n"),
            "{text}"
        );
    }

    #[test]
    fn a_status_from_an_older_daemon_has_nothing_superseded() {
        let older = serde_json::json!({
            "path": "/a/target",
            "used": 1,
            "by_category": crate::store::sizes::Sizes::default(),
        });
        let target: TargetUse = serde_json::from_value(older).expect("an older target");
        assert_eq!(target.superseded, 0);
    }
}
