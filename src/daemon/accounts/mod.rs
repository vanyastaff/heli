//! The daemon's side of the build store (DESIGN.md §6.2): it learns each
//! workspace a run builds in, writes the owner manifest of its build
//! directory, and keeps the sizes the store's directories take.
//!
//! Its parts, each in a module of its own:
//!
//! - [`learning`]: what each run of cargo builds in, and recycling at a
//!   run's start;
//! - [`recycling`]: the look at the worktrees of each repository with an
//!   orphan;
//! - [`records`]: the source-state records of build directories;
//! - sizes, here: the accountant;
//! - [`eviction`]: eviction passes and the trash's deleter.
//!
//! **Sizes** ([`Accounts::disk`]). The accountant, a thread of the daemon's,
//! keeps the sizes of every directory `crate::store::survey` finds, by
//! category. It scans a directory it has no figures for, one whose run has
//! ended (cargo has let go of its lock there, so it no longer changes), and
//! any whose figures are older than [`STALE`], which also catches what a
//! cargo without heli built. It looks again when a run ends, and every
//! [`LOOK`] otherwise; it never takes cargo's locks. `status` reads the
//! figures: one entry per store, its build directories added up, and one
//! per registered `target/` directory.

mod eviction;
mod learning;
mod records;
mod recycling;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::Log;
use super::events::Events;
use crate::config::StoreConfig;
use crate::status::{Disk, LastGc, StoreUse, TargetUse};
use crate::store::ladder::Policy;
use crate::store::registry::Registry;
use crate::store::sizes::Sizes;
use crate::store::{self, Dir, Kind, State, orphan};

pub use learning::Learning;

/// How often the accountant looks for new directories when no run ends.
const LOOK: Duration = Duration::from_secs(60);

/// How old a directory's figures may grow before it is scanned again.
const STALE: Duration = Duration::from_secs(3600);

pub struct Accounts {
    config: StoreConfig,
    policy: Policy,
    /// The state directory, which holds the registry of `target/`
    /// directories (`crate::store::registry`).
    state_dir: PathBuf,
    /// The configuration file, which holds the test seam.
    config_file: PathBuf,
    /// Held while the registry is rewritten: one writer at a time.
    registry: Mutex<()>,
    ledger: Mutex<Ledger>,
    /// Signalled when a directory needs scanning.
    wake: Condvar,
    /// Whether the trash has something new to delete, and its signal.
    trash: Mutex<bool>,
    trash_wake: Condvar,
    /// The last eviction pass that evicted anything.
    last_gc: Mutex<Option<LastGc>>,
    log: Log,
    events: Arc<Events>,
    /// The directories a run was learnt from, its build directory recycled
    /// if it could be: later runs from there never wait.
    prepared: Mutex<BTreeSet<PathBuf>>,
    /// The build directories of the runs in flight that write a
    /// source-state record as they end, once for each run: the accountant
    /// writes none there meanwhile.
    recording: Mutex<Vec<PathBuf>>,
}

/// What the accountant knows.
#[derive(Default)]
struct Ledger {
    /// The stores, as last surveyed.
    stores: Vec<PathBuf>,
    /// Each surveyed directory's figures.
    figures: BTreeMap<PathBuf, Figures>,
    /// Directories to scan again: their runs have ended.
    dirty: BTreeSet<PathBuf>,
    /// A managed runtime ended after a pass may have deferred eviction.
    gc_requested: bool,
}

struct Figures {
    kind: Kind,
    store: Option<PathBuf>,
    sizes: Sizes,
    scanned: Instant,
}

impl Accounts {
    /// The accounts, with their accountant and the trash's deleter started;
    /// `config_file` holds the test seam.
    pub(super) fn start(
        config: StoreConfig,
        state_dir: PathBuf,
        config_file: PathBuf,
        log: Log,
        events: Arc<Events>,
    ) -> Arc<Self> {
        // Only creating the final directory ourselves proves exclusive cache
        // ownership. Existing/default stores keep their previous policy.
        for path in &config.paths {
            if let Err(error) = store::managed::prepare_new(path) {
                log.write(&format!(
                    "cannot prepare cache store {}: {error}",
                    path.display()
                ));
            }
        }
        let accounts = Arc::new(Self {
            policy: Policy::of(&config),
            config,
            state_dir,
            config_file,
            registry: Mutex::new(()),
            ledger: Mutex::new(Ledger::default()),
            wake: Condvar::new(),
            trash: Mutex::new(false),
            trash_wake: Condvar::new(),
            last_gc: Mutex::new(None),
            log,
            events,
            prepared: Mutex::new(BTreeSet::new()),
            recording: Mutex::new(Vec::new()),
        });
        let accountant = Arc::clone(&accounts);
        std::thread::spawn(move || accountant.keep());
        let deleter = Arc::clone(&accounts);
        std::thread::spawn(move || deleter.empty_trash());
        accounts
    }

    /// The last eviction pass that evicted anything.
    pub fn last_gc(&self) -> Option<LastGc> {
        self.last_gc
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Retry after releasing a managed runtime guard. Keep the request in
    /// the wait predicate so a notification before wait cannot be lost.
    pub(super) fn runtime_ended(&self) {
        if !self.policy.active() {
            return;
        }
        self.ledger().gc_requested = true;
        self.wake.notify_all();
    }

    /// The disk the store takes, as last counted.
    pub fn disk(&self) -> Disk {
        let ledger = self.ledger();
        let text = |path: &Path| path.to_string_lossy().into_owned();
        let stores = ledger
            .stores
            .iter()
            .map(|store| {
                let mut sizes = Sizes::default();
                // Only build directories are in a store.
                for figures in ledger.figures.values() {
                    if figures.store.as_deref() == Some(store.as_path()) {
                        sizes += figures.sizes;
                    }
                }
                StoreUse {
                    path: text(store),
                    budget: self.policy.budget,
                    used: sizes.total(),
                    by_category: sizes,
                }
            })
            .collect();
        let of_kinds = |kinds: &[Kind]| -> Vec<TargetUse> {
            ledger
                .figures
                .iter()
                .filter(|(_, figures)| kinds.contains(&figures.kind))
                .map(|(path, figures)| TargetUse {
                    path: text(path),
                    used: figures.sizes.total(),
                    by_category: figures.sizes,
                    shared: figures.kind == Kind::SharedTarget,
                })
                .collect()
        };
        Disk {
            stores,
            targets: of_kinds(&[Kind::Target, Kind::SharedTarget]),
            scratch: of_kinds(&[Kind::Scratch]),
        }
    }

    /// The accountant: looks for worktrees to recycle for, surveys the store
    /// and scans what needs it, for ever.
    fn keep(&self) {
        let mut looked = BTreeSet::new();
        let mut held = BTreeSet::new();
        loop {
            // Each run that ended is in `ended` once: after it ended, the
            // survey below sees every directory it built.
            let ended = {
                let mut ledger = self.ledger();
                ledger.gc_requested = false;
                std::mem::take(&mut ledger.dirty)
            };
            let registry = Registry::load(&self.state_dir);
            self.look(&registry, &mut looked);
            let dirs = store::survey(&self.config, &registry);
            self.released(&dirs, &mut held);
            let stores = store::stores(&self.config, &registry);
            for dir in &dirs {
                let run_ended = ended.iter().any(|path| store::same_path(path, &dir.path));
                let due = run_ended
                    || self
                        .ledger()
                        .figures
                        .get(&dir.path)
                        .is_none_or(|figures| figures.scanned.elapsed() >= STALE);
                if due {
                    let figures = Figures {
                        kind: dir.kind,
                        store: dir.store.clone(),
                        sizes: dir.scan(&self.config),
                        scanned: Instant::now(),
                    };
                    self.ledger().figures.insert(dir.path.clone(), figures);
                }
            }
            let mut ledger = self.ledger();
            ledger.stores = stores;
            ledger
                .figures
                .retain(|path, _| dirs.iter().any(|dir| dir.path == *path));
            drop(ledger);
            if self.policy.active() {
                let now = self.now();
                orphan::keep(&dirs, now);
                if self.over_limits() {
                    self.evict(&registry, now);
                }
            }
            // A run that ended during this pass, or an eviction, needs
            // another one at once.
            let ledger = self.ledger();
            if ledger.dirty.is_empty() && !ledger.gc_requested {
                let _waited = self
                    .wake
                    .wait_timeout(ledger, LOOK)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn recording(&self) -> MutexGuard<'_, Vec<PathBuf>> {
        self.recording
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}
