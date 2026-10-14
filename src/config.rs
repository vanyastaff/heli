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
//! [store]
//! paths = ['D:\.heli\build']        # build stores, the first on a volume
//!                                   # serves it; default <volume>/.heli/build
//! tool-caches = ["tests/trybuild"]  # tool caches inside target/; default none
//! ```
//!
//! Every setting is optional, and a missing file means every default. Keys
//! this build does not know are ignored, so a file written for a newer heli
//! still works with an older one.

use std::io;
use std::num::NonZeroU32;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub pool: PoolConfig,
    pub log: LogConfig,
    pub store: StoreConfig,
}

/// `[store]`: the build store (`crate::store`).
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct StoreConfig {
    /// The stores, absolute; on each volume the first one there serves it.
    pub paths: Vec<PathBuf>,
    /// The tool caches inside a `target/` directory: patterns of paths
    /// relative to it, `/` between components, `*` and `?` within one.
    pub tool_caches: Vec<String>,
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

impl Config {
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
                "store.tool-caches: {pattern:?} is not a path below target/"
            )),
            None => Ok(()),
        }
    }
}
