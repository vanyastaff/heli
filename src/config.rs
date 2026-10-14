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
//! [output]
//! mode = "compact"     # full, compact or quiet; default: full on a
//!                      # terminal, compact without
//! ```
//!
//! Every setting is optional, and a missing file means every default. Keys
//! this build does not know are ignored, so a file written for a newer heli
//! still works with an older one.
//!
//! The client reads `[output]` too, as it chooses a run's output mode
//! (`crate::output`). It ignores a file it cannot use: the daemon does not
//! start with it, and its log says why.

use std::io;
use std::num::NonZeroU32;
use std::path::Path;

use serde::Deserialize;

use crate::output::Mode;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub pool: PoolConfig,
    pub log: LogConfig,
    pub output: OutputConfig,
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
}
