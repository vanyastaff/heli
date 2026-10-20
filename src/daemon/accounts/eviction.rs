//! **Eviction** (docs/build-store.md#eviction). With a budget or a floor
//! configured, the accountant also keeps the orphans' notes
//! (`crate::store::orphan`) at each look, and when its figures are over the
//! budget, or a volume they are on has less free than the floor, it makes an
//! eviction pass (`crate::store::evict`), unless `gc` is making one. Each
//! eviction goes to the event log; `status` shows the last pass that evicted
//! anything (`last_gc`). Its clock is the test seam's in debug builds that set
//! one. A thread of its own deletes what is in the trash, at low I/O priority:
//! after each pass, and every [`LOOK`], which also clears what a `gc` that was
//! stopped left there.
//!
//! **The daemon's log** gets what a pass found: the directories it left
//! alone for runs in flight, each victim it skipped and why, the budget it
//! cannot meet, and what it evicted. A pass that evicts nothing and finds
//! the same as the last one writes nothing: on a host where runs are
//! always in flight, a pass every look wrote the same lines every time,
//! 16.7k a day (#122). Once an hour, one line says how many passes found
//! the same ([`Said`]). While the busy directories alone hold more than the
//! budget's target, the budget cannot be met (`crate::store::evict`), and
//! `status` and `doctor` say so (`unmet_budget`).

use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use super::{Accounts, LOOK};
use crate::status::{Evicted, LastGc, UnmetBudget};
use crate::store::registry::Registry;
use crate::store::{evict, ladder, sizes, trash, volume};
use crate::{clock, config};

/// How long passes that find the same go unlogged before one line says so.
const SAME_FOR: Duration = Duration::from_secs(3600);

/// What the last pass wrote to the daemon's log, so that a pass that
/// evicts nothing and finds the same writes nothing.
pub(super) struct Said {
    found: Found,
    /// When the log last heard of it.
    at: Instant,
    /// The passes since that found the same and wrote nothing.
    repeated: u32,
}

/// What a pass found, as far as the log says it.
#[derive(PartialEq, Eq)]
struct Found {
    runtime_busy: bool,
    /// The victims skipped: path, tier and why.
    skipped: Vec<(PathBuf, u8, String)>,
    budget_unmet: bool,
}

impl Accounts {
    /// The time now for the ladder: the test seam's clock, if it has one.
    pub(super) fn now(&self) -> u64 {
        ladder::now(config::current_seam(&self.config_file).as_ref())
    }

    /// Whether the figures are over the budget, or a volume they are on has
    /// less free than the floor.
    pub(super) fn over_limits(&self) -> bool {
        let ledger = self.ledger();
        let used: u64 = ledger
            .figures
            .values()
            .map(|figures| figures.sizes.total())
            .sum();
        if self.policy.budget.is_some_and(|budget| used > budget) {
            return true;
        }
        let Some(floor) = self.policy.floor else {
            return false;
        };
        let mut roots: Vec<PathBuf> = Vec::new();
        for path in ledger.figures.keys() {
            if let Some(root) = volume::root(path)
                && !roots.contains(&root)
            {
                roots.push(root);
            }
        }
        drop(ledger);
        roots
            .iter()
            .any(|root| volume::free(root).is_some_and(|free| free < floor))
    }

    /// Makes an eviction pass at `now`, unless `gc` is making one: logs
    /// each eviction, remembers the pass if it evicted anything, and has
    /// what it evicted from counted again and the trash emptied.
    pub(super) fn evict(&self, registry: &Registry, now: u64) {
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(evict::lock_file(&self.state_dir));
        let _evicting = match lock.map(|file| file.try_lock().map(|()| file)) {
            Ok(Ok(file)) => file,
            Ok(Err(TryLockError::WouldBlock)) => return,
            Ok(Err(TryLockError::Error(error))) | Err(error) => {
                self.log
                    .write(&format!("cannot take the eviction lock: {error}"));
                return;
            }
        };
        let mut pass = evict::plan(
            &self.config,
            registry,
            &self.state_dir,
            now,
            false,
            evict::Scope::Ladder,
        );
        evict::execute(&mut pass, now);
        let unmet = pass.unmet_budget();
        self.note_budget(unmet);
        let mut lines = Vec::new();
        if pass.runtime_busy {
            lines.push(
                "eviction left alone the directories of workspaces a run is in flight in"
                    .to_owned(),
            );
        }
        for (victim, why) in &pass.skipped {
            lines.push(format!(
                "did not evict {} (tier {}): {why}",
                victim.path.display(),
                victim.tier.number()
            ));
        }
        if let Some(unmet) = unmet {
            lines.push(format!(
                "{}: live directories are kept whole",
                unmet.describe()
            ));
        }
        if !pass.victims.is_empty() {
            let freed: u64 = pass.victims.iter().map(|victim| victim.bytes).sum();
            lines.push(format!(
                "evicted {} victim{}, {}",
                pass.victims.len(),
                if pass.victims.len() == 1 { "" } else { "s" },
                sizes::human(freed)
            ));
        }
        let found = Found {
            runtime_busy: pass.runtime_busy,
            skipped: pass
                .skipped
                .iter()
                .map(|(victim, why)| (victim.path.clone(), victim.tier.number(), why.clone()))
                .collect(),
            budget_unmet: unmet.is_some(),
        };
        self.say(found, &lines, &pass);
        if pass.victims.is_empty() {
            return;
        }
        for victim in &pass.victims {
            self.events.record("evict", evict::event(victim));
        }
        *self.last_gc.lock().unwrap_or_else(PoisonError::into_inner) = Some(LastGc {
            at: clock::now(),
            evicted: pass
                .victims
                .iter()
                .map(|victim| Evicted {
                    path: victim.path.to_string_lossy().into_owned(),
                    tier: victim.tier.number(),
                    bytes: victim.bytes,
                })
                .collect(),
        });
        let mut ledger = self.ledger();
        ledger.dirty.extend(
            pass.victims
                .iter()
                .map(|victim| pass.dirs[victim.dir].dir.path.clone()),
        );
        drop(ledger);
        *self.trash.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.trash_wake.notify_all();
    }

    /// Writes `lines`, what a pass that found `found` has to say, unless
    /// the last pass found the same and this one evicted nothing: then
    /// nothing, or once in [`SAME_FOR`] one line with how many passes found
    /// the same and what that is now.
    fn say(&self, found: Found, lines: &[String], pass: &evict::Pass) {
        let mut said = self.said.lock().unwrap_or_else(PoisonError::into_inner);
        if pass.victims.is_empty()
            && let Some(same) = said.as_mut().filter(|said| said.found == found)
        {
            same.repeated += 1;
            if same.at.elapsed() >= SAME_FOR {
                self.log.write(&format!(
                    "{} eviction passes in the last hour found the same: {}",
                    same.repeated,
                    summary(pass)
                ));
                same.at = Instant::now();
                same.repeated = 0;
            }
            return;
        }
        for line in lines {
            self.log.write(line);
        }
        *said = Some(Said {
            found,
            at: Instant::now(),
            repeated: 0,
        });
    }

    /// Remembers the budget a pass could not meet, since the first pass
    /// that could not, or that the budget can be met again.
    fn note_budget(&self, unmet: Option<evict::UnmetBudget>) {
        let mut current = self
            .unmet_budget
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *current = unmet.map(|unmet| UnmetBudget {
            budget: unmet.budget,
            used: unmet.used,
            busy_dirs: unmet.busy_dirs,
            busy_bytes: unmet.busy_bytes,
            since: current.map_or_else(clock::now, |was| was.since),
        });
    }

    /// The figures are within the limits: no pass is needed, and the budget
    /// is met.
    pub(super) fn budget_met(&self) {
        self.note_budget(None);
    }

    /// The trash's deleter: deletes what is in every trash, at low I/O
    /// priority, when a pass has put something there and every [`LOOK`].
    pub(super) fn empty_trash(&self) {
        trash::lower_io_priority();
        let mut last_error = String::new();
        loop {
            let registry = Registry::load(&self.state_dir);
            for trash in trash::all(&self.config, &registry, &self.state_dir) {
                if let Err(error) = trash::empty(&trash) {
                    let error = format!("cannot empty {} yet: {error}", trash.display());
                    if error != last_error {
                        self.log.write(&error);
                        last_error = error;
                    }
                }
            }
            let mut pending = self.trash.lock().unwrap_or_else(PoisonError::into_inner);
            if !*pending {
                pending = self
                    .trash_wake
                    .wait_timeout(pending, LOOK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            *pending = false;
        }
    }
}

/// What a pass found, in one line: `over budget by 286.0 GiB (386.0 GiB
/// used of 100.0 GiB); 18 busy directories hold 200.0 GiB; 9 victims
/// skipped; nothing evicted`.
fn summary(pass: &evict::Pass) -> String {
    let mut parts = Vec::new();
    if let Some(budget) = pass.policy.budget
        && pass.used > budget
    {
        parts.push(format!(
            "over budget by {} ({} used of {})",
            sizes::human(pass.used - budget),
            sizes::human(pass.used),
            sizes::human(budget)
        ));
    }
    for (root, need) in &pass.goals.floors {
        if *need > 0 {
            parts.push(format!(
                "{} under its floor by {}",
                root.display(),
                sizes::human(*need)
            ));
        }
    }
    parts.push(format!(
        "{} busy director{} hold {}",
        pass.busy_dirs,
        if pass.busy_dirs == 1 { "y" } else { "ies" },
        sizes::human(pass.busy_bytes)
    ));
    if !pass.skipped.is_empty() {
        parts.push(format!(
            "{} victim{} skipped",
            pass.skipped.len(),
            if pass.skipped.len() == 1 { "" } else { "s" }
        ));
    }
    if pass.victims.is_empty() {
        parts.push("nothing evicted".to_owned());
    }
    parts.join("; ")
}
