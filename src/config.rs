//! The per-user configuration file (DESIGN.md §5, "Configuration"): TOML,
//! which the daemon reads as it starts. Where it is: `crate::locations`.
//!
//! ```toml
//! [pool]
//! slots = 16   # the host pool's size; default: the number of logical CPUs
//! ```
//!
//! Every setting is optional, and a missing file means every default. Keys
//! this build does not know are ignored, so a file written for a newer heli
//! still works with an older one.

use std::io;
use std::num::NonZeroU32;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub pool: PoolConfig,
}

/// `[pool]`: the host pool (`crate::pool`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// N, the pool's slots.
    pub slots: Option<NonZeroU32>,
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
