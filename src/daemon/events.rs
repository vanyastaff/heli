//! The daemon's event log (DESIGN.md §8): what it decided about runs and
//! the host pool, one JSON object per line, for people and tools.
//!
//! Each event has `at` (seconds since 1970, UTC) and `event`, then fields of
//! its own:
//!
//! - `admit`: a run was admitted: `run`, `worktree`, `argv`;
//! - `withhold`: the memory governor withheld a slot: `withheld` (after
//!   it), `available`, `commit_available` (bytes) and `low_watermark`;
//! - `return`: it returned every withheld slot: `slots`, `available`,
//!   `commit_available` and `high_watermark`;
//! - `top-up`: slots given back to the pool at quiescence;
//! - `drain`: surplus slots taken out of the pool. Both have `slots`,
//!   `free` (after it) and `target`;
//! - `recycle`: a worktree got an orphan's build directory
//!   (`crate::store::recycle`): `trigger` (`run`, its first run; `hook`,
//!   git's `post-checkout` hook; `look`, the daemon's look at a
//!   repository's worktrees), `workspace` and `to`, the new workspace and
//!   the build directory's path now, `donor` and `from`, the removed
//!   workspace and the path it had, and `invalidated`, the units whose
//!   fingerprints the path scan deleted. The hook writes its event itself,
//!   to the log of its logon session's daemon.
//!
//! It is `events.log` in the state directory (`events-<session>.log` on
//! Windows). Past its size limit it rotates: it becomes `<name>.1`, replacing
//! the one before, and a new one starts. A line that cannot be written is
//! lost; the daemon goes on.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde_json::{Map, Value};

use super::registry::RunId;
use crate::clock;
use crate::config::Config;
use crate::locations::Locations;

/// The size limit when the configuration sets none: 1 MB.
pub const DEFAULT_MAX: u64 = 1 << 20;

pub struct Events {
    path: PathBuf,
    /// Bytes past which the log rotates.
    max: u64,
    /// The current file, opened at its first event.
    file: Mutex<Option<File>>,
}

impl Events {
    /// The log at `path`, rotating past `max` bytes.
    pub fn new(path: PathBuf, max: u64) -> Self {
        Self {
            path,
            max,
            file: Mutex::new(None),
        }
    }

    /// The log of `locations`, of the size `config` sets.
    pub fn configured(locations: &Locations, config: &Config) -> Self {
        let max = config
            .event_log
            .max_kb
            .map_or(DEFAULT_MAX, |kb| kb.get().saturating_mul(1024));
        Self::new(locations.event_log(), max)
    }

    /// Logs the admission of run `run`, with `fields` (a JSON object).
    pub fn admit(&self, run: RunId, fields: Value) {
        let mut admission = Map::new();
        admission.insert("run".to_owned(), run.to_string().into());
        if let Value::Object(fields) = fields {
            admission.extend(fields);
        }
        self.record("admit", Value::Object(admission));
    }

    /// Logs `event`, with `fields` (a JSON object) besides `at` and `event`.
    pub fn record(&self, event: &str, fields: Value) {
        let mut line = Map::new();
        line.insert("at".to_owned(), clock::now().into());
        line.insert("event".to_owned(), event.into());
        if let Value::Object(fields) = fields {
            line.extend(fields);
        }
        let mut text = Value::Object(line).to_string();
        text.push('\n');

        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if file.is_none() {
            *file = append(&self.path);
        }
        let Some(current) = file.as_mut() else {
            return;
        };
        let _ = current.write_all(text.as_bytes());
        if current
            .metadata()
            .is_ok_and(|metadata| metadata.len() > self.max)
        {
            *file = None;
            let _ = std::fs::rename(&self.path, rotated(&self.path));
        }
    }
}

/// `path` opened for appending, created if needed.
fn append(path: &Path) -> Option<File> {
    File::options().create(true).append(true).open(path).ok()
}

/// Where `path` goes when it rotates: `<path>.1`.
fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}
