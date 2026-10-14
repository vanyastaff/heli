//! **Eviction** (DESIGN.md §6.3). With a budget or a floor configured, the
//! accountant also keeps the orphans' notes (`crate::store::orphan`) at
//! each look, and when its figures are over the budget, or a volume they
//! are on has less free than the floor, it makes an eviction pass
//! (`crate::store::evict`), unless `gc` is making one. Each eviction goes
//! to the event log; `status` shows the last pass that evicted anything
//! (`last_gc`). Its clock is the test seam's in debug builds that set one.
//! A thread of its own deletes what is in the trash, at low I/O priority:
//! after each pass, and every [`LOOK`], which also clears what a `gc`
//! that was stopped left there.

use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::sync::PoisonError;

use super::{Accounts, LOOK};
use crate::status::{Evicted, LastGc};
use crate::store::ladder;
use crate::store::registry::Registry;
use crate::store::{evict, trash, volume};
use crate::{clock, config};

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
        let mut pass = evict::plan(&self.config, registry, now, false);
        evict::execute(&mut pass, now);
        for (victim, why) in &pass.skipped {
            self.log.write(&format!(
                "did not evict {} (tier {}): {why}",
                victim.path.display(),
                victim.tier.number()
            ));
        }
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

    /// The trash's deleter: deletes what is in every trash, at low I/O
    /// priority, when a pass has put something there and every [`LOOK`].
    pub(super) fn empty_trash(&self) {
        trash::lower_io_priority();
        let mut last_error = String::new();
        loop {
            let registry = Registry::load(&self.state_dir);
            for trash in trash::all(&self.config, &registry) {
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
