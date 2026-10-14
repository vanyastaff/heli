//! The memory governor and the bounded top-up (DESIGN.md §4): one thread of
//! the daemon that watches the machine's memory and keeps the host pool at
//! its target of free slots (`crate::pool::target`).
//!
//! Every [`TICK`], it **drains** the free slots above the target: a surplus,
//! whoever gave it.
//!
//! Every [`SAMPLE`] of its clock, it samples memory (`crate::memory`):
//!
//! - **Withhold.** Below the low watermark (available physical memory or,
//!   on Windows, available commit), it takes one free slot out of the pool
//!   and holds it, leaving the pool at least one slot. Running jobs are never
//!   interrupted: it only takes slots that are free.
//! - **Return.** Above the high watermark (both figures), it gives every
//!   withheld slot back. Between the two, nothing changes: the gap keeps it
//!   from swinging back and forth.
//! - **Top up.** Once the pool has been quiet for [`QUIET`] ([`quiet`]), it
//!   raises the free slots to the target, giving back the slots that
//!   processes took with them when they died.
//!
//! Each of these goes to the event log (`super::events`). `status` shows a
//! sample once the governor has acted on it.
//!
//! Its clock is the time since the daemon started, or in debug builds the
//! test seam's, as are the memory figures (`crate::config::Seam`).

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::{Config, MemoryConfig, Seam};
use crate::pool::{Adjustment, Pool};
use crate::{config, memory, processes, status};

use super::Log;
use super::events::Events;
use super::registry::Registry;

/// How often the governor looks.
const TICK: Duration = Duration::from_millis(100);

/// How often, by its clock, it samples memory: milliseconds.
const SAMPLE: u64 = 1000;

/// How long, by its clock, the pool must have been quiet for a top-up:
/// milliseconds.
const QUIET: u64 = 2000;

/// A megabyte, as the configuration counts.
const MB: u64 = 1 << 20;

/// The low watermark is never below this by default: 6 GB.
const LOW_FLOOR: u64 = 6 << 30;

/// The high watermark is the low one plus this by default: 4 GB.
const BAND: u64 = 4 << 30;

pub struct Governor {
    config: MemoryConfig,
    /// The configuration file, which holds the test seam.
    config_file: PathBuf,
    /// The seam as the daemon started with it.
    seam: Seam,
    /// The last sample, with its watermarks, for `status`.
    memory: Mutex<Option<status::Memory>>,
}

impl Governor {
    /// A governor with the watermarks `config` sets, read from
    /// `config_file`; the error says why they cannot be used.
    pub fn new(config: &Config, config_file: PathBuf) -> Result<Self, String> {
        let seam = config.seam();
        // Without a reading, the default low watermark is its floor.
        let total = read(&seam).map_or(0, |sample| sample.total);
        Watermarks::new(&config.memory, total)?;
        Ok(Self {
            config: config.memory,
            config_file,
            seam,
            memory: Mutex::new(None),
        })
    }

    /// The last sample and its watermarks; `None` before the first, or while
    /// memory cannot be read.
    pub fn memory(&self) -> Option<status::Memory> {
        self.memory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Governs `pool` until the daemon exits, recording what it does in
    /// `events`, and its errors in `log`. `registry` holds the heli runs in
    /// flight, and `lent` tells the slots lent to their run pools (Σ lent).
    pub(super) fn run(
        &self,
        pool: &Pool,
        registry: &Registry,
        events: &Events,
        log: &Log,
        lent: impl Fn() -> u32,
    ) -> ! {
        let started = Instant::now();
        let mut seam = self.seam;
        let mut last_sample: Option<u64> = None;
        let mut quiet_since: Option<u64> = None;
        let mut unreadable = false;
        loop {
            if let Some(current) = config::current_seam(&self.config_file) {
                seam = current;
            }
            let now = seam.clock_ms.unwrap_or_else(|| {
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            match pool.drain(&lent) {
                Ok(Some(drain)) => events.record("drain", adjustment(drain)),
                Ok(None) => {}
                Err(error) => log.write(&format!("cannot drain the pool: {error}")),
            }
            if last_sample.is_none_or(|last| now >= last.saturating_add(SAMPLE)) {
                last_sample = Some(now);
                let sample = read(&seam);
                if let Ok(sample) = &sample {
                    self.govern(sample, pool, events, log);
                }
                quiet_since = quiet(pool, registry).then(|| quiet_since.unwrap_or(now));
                if quiet_since.is_some_and(|since| now.saturating_sub(since) >= QUIET) {
                    match pool.top_up(&lent) {
                        Ok(Some(top_up)) => events.record("top-up", adjustment(top_up)),
                        Ok(None) => {}
                        Err(error) => log.write(&format!("cannot top the pool up: {error}")),
                    }
                }
                self.show(sample, &mut unreadable, log);
            }
            std::thread::sleep(TICK);
        }
    }

    /// Withholds a slot, or returns them, as `sample` asks.
    fn govern(&self, sample: &memory::Sample, pool: &Pool, events: &Events, log: &Log) {
        // A total that changed (the seam's) can only make the watermarks
        // unusable; the governor then waits for usable ones.
        let Ok(marks) = Watermarks::new(&self.config, sample.total) else {
            return;
        };
        let figures = json!({
            "available": sample.available,
            "commit_available": sample.commit_available,
        });
        if marks.pressed(sample) {
            match pool.withhold() {
                Ok(Some(withheld)) => events.record(
                    "withhold",
                    with(
                        figures,
                        json!({"withheld": withheld, "low_watermark": marks.low}),
                    ),
                ),
                Ok(None) => {}
                Err(error) => log.write(&format!("cannot withhold a slot: {error}")),
            }
        } else if marks.relieved(sample) {
            match pool.return_withheld() {
                Ok(0) => {}
                Ok(returned) => events.record(
                    "return",
                    with(
                        figures,
                        json!({"slots": returned, "high_watermark": marks.high}),
                    ),
                ),
                Err(error) => log.write(&format!("cannot return the withheld slots: {error}")),
            }
        }
    }

    /// Makes `sample` what `status` shows. Memory that cannot be read shows
    /// as none, and is logged when it stops being readable (`unreadable`
    /// says whether it was already).
    fn show(&self, sample: std::io::Result<memory::Sample>, unreadable: &mut bool, log: &Log) {
        let shown = match sample {
            Ok(sample) => {
                *unreadable = false;
                Watermarks::new(&self.config, sample.total)
                    .ok()
                    .map(|marks| status::Memory {
                        total: sample.total,
                        available: sample.available,
                        commit_available: sample.commit_available,
                        low_watermark: marks.low,
                        high_watermark: marks.high,
                    })
            }
            Err(error) => {
                if !*unreadable {
                    log.write(&format!("cannot read the memory: {error}"));
                }
                *unreadable = true;
                None
            }
        };
        *self.memory.lock().unwrap_or_else(PoisonError::into_inner) = shown;
    }
}

/// Whether the pool is quiet now: no heli run is in flight, and either no
/// other process holds the pool open, or none compiles. A slot missing from
/// a quiet pool can only be one a process took with it when it died.
fn quiet(pool: &Pool, registry: &Registry) -> bool {
    // Cheapest first; a scan that fails counts as compiling.
    registry.snapshot().is_empty()
        && (!pool.held_by_others() || !processes::compiling().unwrap_or(true))
}

/// An adjustment of the pool, as the event log has it.
fn adjustment(adjustment: Adjustment) -> Value {
    json!({
        "slots": adjustment.slots,
        "free": adjustment.free,
        "target": adjustment.target,
    })
}

/// `fields` and `more`, two JSON objects, as one.
fn with(mut fields: Value, more: Value) -> Value {
    if let (Value::Object(fields), Value::Object(more)) = (&mut fields, more) {
        fields.extend(more);
    }
    fields
}

/// The memory: the seam's, else the machine's.
fn read(seam: &Seam) -> std::io::Result<memory::Sample> {
    match seam.memory {
        Some(injected) => Ok(memory::Sample {
            total: injected.total_mb.saturating_mul(MB),
            available: injected.available_mb.saturating_mul(MB),
            commit_available: injected.commit_available_mb.map(|mb| mb.saturating_mul(MB)),
        }),
        None => memory::sample(),
    }
}

/// The two watermarks, in bytes.
#[derive(Debug, Clone, Copy)]
struct Watermarks {
    low: u64,
    high: u64,
}

impl Watermarks {
    /// The watermarks `config` sets on a machine with `total` bytes of RAM.
    fn new(config: &MemoryConfig, total: u64) -> Result<Self, String> {
        let low = config
            .low_watermark_mb
            .map_or((total / 10).max(LOW_FLOOR), |mb| mb.saturating_mul(MB));
        let high = config
            .high_watermark_mb
            .map_or(low.saturating_add(BAND), |mb| mb.saturating_mul(MB));
        if high <= low {
            return Err(format!(
                "invalid configuration: [memory] high_watermark_mb ({} MB) must be above \
                 the low watermark ({} MB)",
                high / MB,
                low / MB
            ));
        }
        Ok(Self { low, high })
    }

    /// Whether memory is short: available physical memory or available
    /// commit below the low watermark.
    fn pressed(&self, sample: &memory::Sample) -> bool {
        sample.available < self.low
            || sample
                .commit_available
                .is_some_and(|commit| commit < self.low)
    }

    /// Whether memory has recovered: both above the high watermark.
    fn relieved(&self, sample: &memory::Sample) -> bool {
        sample.available > self.high
            && sample
                .commit_available
                .is_none_or(|commit| commit > self.high)
    }
}
