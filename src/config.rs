//! The per-user configuration file (DESIGN.md §5, "Configuration"): TOML,
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
//! ```
//!
//! Every setting is optional, and a missing file means every default. Keys
//! this build does not know are ignored, so a file written for a newer heli
//! still works with an older one.
//!
//! **The test seam.** Debug builds also read a `[test]` table ([`Seam`]):
//! memory figures and a clock for the memory governor, which tests set
//! instead of the machine's. The daemon reads it again each time the
//! governor looks ([`current_seam`]), so a test can change them while it
//! runs. Release builds ignore the table.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub pool: PoolConfig,
    pub log: LogConfig,
    pub memory: MemoryConfig,
    pub event_log: EventLogConfig,
    #[cfg(debug_assertions)]
    test: Seam,
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

/// `[event_log]`: the daemon's event log.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct EventLogConfig {
    /// The size, in kilobytes of 1024 bytes, past which it rotates.
    pub max_kb: Option<NonZeroU64>,
}

/// The test seam (debug builds only): what the memory governor reads here
/// instead of asking the machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Seam {
    /// The governor's clock, in milliseconds; `None`: the real one.
    pub clock_ms: Option<u64>,
    /// `[test.memory]`: memory figures; `None`: the machine's.
    pub memory: Option<InjectedMemory>,
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
    /// Reads the configuration at `path`; no file there is the default
    /// configuration. The error says what is wrong, and where.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        toml::from_str(&text)
            .map_err(|error| format!("invalid configuration {}: {error}", path.display()))
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

/// The test seam the configuration file at `path` holds now; `None` when it
/// cannot be read, and always in a release build, which has no seam.
pub fn current_seam(path: &Path) -> Option<Seam> {
    if cfg!(debug_assertions) {
        Config::load(path).ok().map(|config| config.seam())
    } else {
        None
    }
}
