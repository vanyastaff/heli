//! The per-user configuration file (docs/runs.md, "Configuration"): TOML,
//! which the daemon reads as it starts. Where it is: `crate::locations`.
//!
//! ```toml
//! [pool]
//! slots = 16   # the host pool's size; default: the number of logical CPUs
//!
//! [log]
//! runs = 200           # run logs kept; default 200
//! bytes = 2147483648   # their total size; default 2 GiB
//!
//! [memory]     # the memory governor's watermarks, in megabytes (2^20 bytes)
//! low_watermark_mb = 6144    # default: the larger of 10 % of RAM and 6 GB
//! high_watermark_mb = 10240  # default: the low watermark plus 4 GB
//!
//! [event_log]
//! max_kb = 1024   # the event log's size before it rotates
//!
//! [advisories]
//! fetch_interval_minutes = 10   # how stale cargo audit / deny's database
//!                               # may be before a check fetches; default 0
//!
//! [output]
//! mode = "compact"     # full, compact or quiet; default: full on a
//!                      # terminal, compact without
//!
//! [cargo]
//! home = "isolated"    # shared (default), or a full private home per worktree
//!
//! [profiles]                  # tool profiles (crate::profiles)
//! trust_repository = false    # let .config/heli.toml loosen them
//! [[profiles.tool]]
//! tool = "xtask"
//! args = ["check-changed"]
//! class = "verify"
//!
//! [store]
//! paths = ['D:\.heli\build']        # build stores, the first on a volume
//!                                   # serves it; default <volume>/.heli/build
//! tool_caches = ["tests/trybuild"]  # tool caches inside target/, besides the
//!                                   # built-in llvm-cov-target; default none
//! budget_mb = 153600   # B, for every build directory and registered
//!                      # target/ directory together; default none
//! floor_mb = 51200     # F, the free space kept on each volume; default none
//! unit_hours = 24      # the eviction ages (crate::store::ladder), in hours;
//! orphan_hours = 24    # these are the defaults
//! idle_hours = 6
//! dir_hours = 168
//! scratch_hours = 1    # tier 0: scratch copies cargo-mutants left behind
//! ```
//!
//! Every setting is optional, and a missing file means every default. Keys
//! this build does not know are ignored, so a file written for a newer heli
//! still works with an older one.
//!
//! The client reads `[output]`, `[cargo]` and `[profiles]` before preparing a run.
//! Invalid Cargo policy or TOML is terminal: neither may silently disable
//! isolation. Invalid daemon-owned settings still permit direct fallback,
//! preserving the selected home; an invalid output setting uses the default.
//!
//! **The test seam.** Debug builds also read a `[test]` table ([`Seam`]):
//! memory figures, and a clock for the memory governor and the eviction
//! ladder, which tests set instead of the machine's, and the grace periods
//! of a run without a client (`crate::daemon::detached`). The daemon reads
//! it again each time the governor, the ladder or the watch on detached
//! runs looks ([`current_seam`]), so a test can change them while it runs.
//! Release builds ignore the table.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::output::Mode;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub cargo: CargoConfig,
    pub pool: PoolConfig,
    pub log: LogConfig,
    pub output: OutputConfig,
    pub memory: MemoryConfig,
    pub event_log: EventLogConfig,
    pub store: StoreConfig,
    pub advisories: AdvisoriesConfig,
    #[cfg(debug_assertions)]
    test: Seam,
}

/// Only settings the client owns. Daemon settings are validated on startup;
/// their failure must not prevent direct execution with the selected home.
#[derive(Debug, Default)]
pub struct ClientConfig {
    pub cargo: CargoConfig,
    pub output: OutputConfig,
    pub profiles: crate::profiles::Profiles,
}

impl ClientConfig {
    fn parse(text: &str) -> Result<Self, String> {
        let value: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
        let cargo: CargoConfig = value
            .get("cargo")
            .cloned()
            .map(toml::Value::try_into)
            .transpose()
            .map_err(|error: toml::de::Error| error.to_string())?
            .unwrap_or_default();
        // Output historically fell back to its default when config loading
        // failed. Preserve that behavior without discarding Cargo policy.
        let output: OutputConfig = value
            .get("output")
            .cloned()
            .and_then(|value| value.try_into().ok())
            .unwrap_or_default();
        let profiles = value
            .get("profiles")
            .cloned()
            .map(toml::Value::try_into)
            .transpose()
            .map_err(|error: toml::de::Error| format!("[profiles]: {error}"))?
            .unwrap_or_default();
        Ok(Self {
            cargo,
            output,
            profiles,
        })
    }
}

/// The writable Cargo home each managed invocation uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CargoHome {
    #[default]
    Shared,
    Isolated,
}

/// Client-side policy; changing it does not require restarting the daemon.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CargoConfig {
    pub home: CargoHome,
}

/// `[store]`: the build store (`crate::store`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StoreConfig {
    /// The stores, absolute; on each volume the first one there serves it.
    pub paths: Vec<PathBuf>,
    /// The tool caches inside a `target/` directory: patterns of paths
    /// relative to it, `/` between components, `*` and `?` within one.
    pub tool_caches: Vec<String>,
    /// B, in megabytes of 2^20 bytes: what every build directory and
    /// registered `target/` directory may take together. `None`: no budget.
    pub budget_mb: Option<u64>,
    /// F, in megabytes: the free space heli keeps on each volume that holds
    /// a store or a registered `target/` directory. `None`: no floor.
    pub floor_mb: Option<u64>,
    /// Tier 1: incremental units untouched this many hours; default 24.
    pub unit_hours: Option<u64>,
    /// Tier 2: orphans unclaimed this many hours; default 24.
    pub orphan_hours: Option<u64>,
    /// Tiers 3 and 4: directories idle this many hours; default 6.
    pub idle_hours: Option<u64>,
    /// Tier 5: whole build directories idle this many hours; default 168.
    pub dir_hours: Option<u64>,
    /// Tier 0: scratch copies cargo tools left behind, untouched this many
    /// hours; default 1 (`crate::store::scratch`).
    pub scratch_hours: Option<u64>,
}

/// Tool caches heli knows without configuration: cargo-llvm-cov's own
/// target directory inside `target/`.
pub const BUILTIN_TOOL_CACHES: [&str; 1] = ["llvm-cov-target"];

impl StoreConfig {
    /// The tool-cache patterns of a `target/` directory: the built-in ones
    /// ([`BUILTIN_TOOL_CACHES`]), then the configuration's.
    pub fn tool_cache_patterns(&self) -> Vec<String> {
        BUILTIN_TOOL_CACHES
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .chain(self.tool_caches.iter().cloned())
            .collect()
    }
}

/// `[pool]`: the host pool (`crate::pool`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// N, the pool's slots.
    pub slots: Option<NonZeroU32>,
}

/// `[log]`: how much of the run log the daemon keeps (`crate::runlog`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// How many records.
    pub runs: Option<usize>,
    /// How many bytes of records.
    pub bytes: Option<u64>,
}

/// `[output]`: what the client prints of a run (`crate::output`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// The output mode when neither a flag nor `HELI_OUTPUT` chooses one.
    pub mode: Option<Mode>,
}

/// `[memory]`: the memory governor's watermarks (`crate::daemon::governor`),
/// in megabytes of 2^20 bytes.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Below it, slots are withheld.
    pub low_watermark_mb: Option<u64>,
    /// Above it, they are returned; it must be above the low watermark.
    pub high_watermark_mb: Option<u64>,
}

/// `[advisories]`: how often the daemon fetches the advisory databases of
/// `cargo audit` and `cargo deny` (`crate::daemon::advisory`).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct AdvisoriesConfig {
    /// A fetch younger than this is fresh: checks in between reuse a pass
    /// without asking the network. Default 0: every check fetches first.
    pub fetch_interval_minutes: u64,
}

/// `[event_log]`: the daemon's event log.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct EventLogConfig {
    /// The size, in kilobytes of 1024 bytes, past which it rotates.
    pub max_kb: Option<NonZeroU64>,
}

/// The test seam (debug builds only): what the memory governor and the
/// eviction ladder read here instead of asking the machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Seam {
    /// The clock, in milliseconds since 1970; `None`: the real one. The
    /// memory governor counts only the time between its readings; the
    /// eviction ladder takes it as the time now (`crate::store::ladder`).
    pub clock_ms: Option<u64>,
    /// `[test.memory]`: memory figures; `None`: the machine's.
    pub memory: Option<InjectedMemory>,
    /// How long a run goes without a client before it is weighed
    /// background, in milliseconds; `None`:
    /// `crate::daemon::detached::DEMOTE_AFTER`.
    pub detached_demote_ms: Option<u64>,
    /// How long a run goes without a client before it is cancelled while
    /// another run waits for a slot, in milliseconds; `None`:
    /// `crate::daemon::detached::CANCEL_AFTER`.
    pub detached_cancel_ms: Option<u64>,
}

/// Memory figures in megabytes, injected for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct InjectedMemory {
    pub total_mb: u64,
    pub available_mb: u64,
    #[serde(default)]
    pub commit_available_mb: Option<u64>,
}

impl Config {
    /// Reads client policy independently of daemon-owned configuration.
    pub fn load_client(path: &Path) -> Result<ClientConfig, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ClientConfig::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        ClientConfig::parse(&text)
            .map_err(|error| format!("invalid configuration {}: {error}", path.display()))
    }

    /// Reads the configuration at `path`; no file there is the default
    /// configuration. The error says what is wrong, and where.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let config: Self = toml::from_str(&text)
            .map_err(|error| format!("invalid configuration {}: {error}", path.display()))?;
        config
            .store
            .check()
            .map_err(|error| format!("invalid configuration {}: {error}", path.display()))?;
        Ok(config)
    }

    /// The test seam this configuration holds: always none in a release
    /// build.
    pub fn seam(&self) -> Seam {
        #[cfg(debug_assertions)]
        return self.test;
        #[cfg(not(debug_assertions))]
        return Seam::default();
    }
}

impl StoreConfig {
    /// Refuses a store that is not an absolute path, and a tool-cache
    /// pattern that is not a relative path below `target/`.
    fn check(&self) -> Result<(), String> {
        if let Some(store) = self.paths.iter().find(|store| !store.is_absolute()) {
            return Err(format!(
                "store.paths: {} is not an absolute path",
                store.display()
            ));
        }
        let below = |pattern: &str| {
            !pattern.is_empty()
                && Path::new(pattern)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
        };
        match self.tool_caches.iter().find(|pattern| !below(pattern)) {
            Some(pattern) => Err(format!(
                "store.tool_caches: {pattern:?} is not a path below target/"
            )),
            None => Ok(()),
        }
    }
}

/// The test seam the configuration file at `path` holds now; `None` when it
/// cannot be read, and always in a release build, which has no seam.
pub fn current_seam(path: &Path) -> Option<Seam> {
    if cfg!(debug_assertions) {
        Config::load(path).ok().map(|config| config.seam())
    } else {
        None
    }
}

#[cfg(test)]
mod client_policy_tests {
    use super::{CargoHome, ClientConfig, Config};

    #[test]
    fn daemon_configuration_errors_preserve_selected_client_home() {
        let text = "[cargo]\nhome='isolated'\n[memory]\nlow_watermark_mb='lots'\n[pool]\nslots='many'\n[store]\npaths=['relative']\n";
        let client = ClientConfig::parse(text).unwrap();
        assert_eq!(client.cargo.home, CargoHome::Isolated);
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn invalid_cargo_policy_and_invalid_toml_are_terminal() {
        for text in [
            "[cargo]\nhome='unknown'",
            "[cargo]\nhome=42",
            "cargo='isolated'",
            "[memory\nlow_watermark_mb=1",
        ] {
            assert!(ClientConfig::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn invalid_output_uses_default_without_losing_isolation() {
        let client =
            ClientConfig::parse("[cargo]\nhome='isolated'\n[output]\nmode='unknown'").unwrap();
        assert_eq!(client.cargo.home, CargoHome::Isolated);
        assert!(client.output.mode.is_none());
    }
}
