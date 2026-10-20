//! Detached runs (docs/runs.md, "Detach"): a run whose clients have all gone
//! (killed, or an agent that gave up on it) goes on to its end, since the
//! same command attaches to it again. But one that computes for nobody
//! while the pool is contended holds its slots and its share like any other
//! run (#121). So the daemon watches the runs no client follows, every
//! [`POLL`]:
//!
//! - After [`DEMOTE_AFTER`] without a client, a run is weighed `background`
//!   (`crate::pool::broker`) until a client attaches again, which restores
//!   its own priority: runs somebody waits for take its slots first.
//! - After [`CANCEL_AFTER`] without a client, while another run waits for a
//!   slot, it is cancelled: its record says so on its stderr
//!   ([`record_line`]), as do `status` (`exit.detached`) and the daemon's
//!   log, and the event log gets a `cancel` event.
//!
//! `status` shows a run without a client with `detached_since`. In debug
//! builds the test seam (`crate::config::Seam`) shortens both periods.

use std::path::Path;
use std::time::Duration;

use super::Daemon;
use super::registry::Detached;
use crate::config;
use crate::priority::Priority;
use crate::run_id::RunId;

/// How long a run goes without a client before it is weighed background.
pub const DEMOTE_AFTER: Duration = Duration::from_secs(120);
/// How long a run goes without a client before it is cancelled, while
/// another run waits for a slot.
pub const CANCEL_AFTER: Duration = Duration::from_secs(600);
/// How often the daemon looks at the runs no client follows.
const POLL: Duration = Duration::from_secs(1);

impl Daemon {
    /// Watches the runs no client follows, for as long as the daemon lives
    /// (see the module docs); `config` is the configuration file, read for
    /// the test seam at each look.
    pub(super) fn watch_detached(&self, config: &Path) -> ! {
        loop {
            std::thread::sleep(POLL);
            let seam = config::current_seam(config).unwrap_or_default();
            let demote_after = seam
                .detached_demote_ms
                .map_or(DEMOTE_AFTER, Duration::from_millis);
            let cancel_after = seam
                .detached_cancel_ms
                .map_or(CANCEL_AFTER, Duration::from_millis);
            for run in self.registry.unattended() {
                if run.since >= cancel_after {
                    let waiting = self.broker.waiting(run.id);
                    if waiting > 0 {
                        self.cancel_detached(
                            run.id,
                            Detached {
                                unattended: run.since,
                                waiting,
                            },
                        );
                        continue;
                    }
                }
                if !run.demoted && run.since >= demote_after && self.registry.demote(run.id) {
                    self.broker.reweigh(run.id, Priority::Background);
                    self.log.write(&format!(
                        "{}: no client for {}: background until one attaches",
                        run.id,
                        seconds(run.since)
                    ));
                }
            }
        }
    }

    /// Cancels run `id` for having no client, as `detached` says, unless a
    /// client attached meanwhile.
    fn cancel_detached(&self, id: RunId, detached: Detached) {
        let Ok(tree) = self.registry.cancel_detached(id, detached) else {
            return;
        };
        match tree.map_or(Ok(()), |tree| tree.kill()) {
            Ok(()) => {
                self.log.write(&format!(
                    "cancelled {id}: no client for {} while {} waited for slots",
                    seconds(detached.unattended),
                    runs(detached.waiting)
                ));
                self.events.record(
                    "cancel",
                    serde_json::json!({
                        "run": id.to_string(),
                        "why": "no client",
                        "unattended_s": detached.unattended.as_secs(),
                        "waiting": detached.waiting,
                    }),
                );
            }
            Err(error) => self.log.write(&format!("cannot cancel {id}: {error}")),
        }
    }
}

/// The line the record of a run cancelled for having no client ends with,
/// on its stderr: `cargo-heli: r17: cancelled: no client followed it for
/// 600 s while 3 runs waited for slots`.
pub fn record_line(id: RunId, detached: Detached) -> String {
    format!(
        "cargo-heli: {id}: cancelled: no client followed it for {} while {} waited for slots\n",
        seconds(detached.unattended),
        runs(detached.waiting)
    )
}

/// `72 s`.
fn seconds(duration: Duration) -> String {
    format!("{} s", duration.as_secs())
}

/// `1 run` or `3 runs`.
fn runs(count: usize) -> String {
    format!("{count} run{}", if count == 1 { "" } else { "s" })
}
