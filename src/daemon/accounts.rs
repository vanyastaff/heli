//! The daemon's side of the build store (DESIGN.md §6.2): it learns each
//! workspace a run builds in, writes the owner manifest of its build
//! directory, and keeps the sizes the store's directories take.
//!
//! **Learning** ([`Accounts::learn`]). A run of cargo, a run whose program
//! is named `cargo` (as heli runs a cargo subcommand, and an `exec` of
//! cargo), starts, beside it in the background,
//! `cargo metadata --format-version 1 --no-deps` as the run would: its
//! program, in its directory, with its environment. No other program is
//! ever started with these arguments, not even one standing in for cargo
//! under another name. That names
//! the workspace root, its `target/` directory and its build directory, as
//! the run's configuration places them. The `target/` directory is
//! registered (`crate::store::registry`). When the build directory is in a
//! store heli manages (`crate::store::store_of`), heli writes its owner
//! manifest (`crate::store::owner`), with the run's start as the last run
//! and the repository git names for the workspace. Nothing of this delays
//! the run, and nothing of it can fail the run: what goes wrong goes to the
//! daemon's log.
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
//!
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

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::Log;
use super::events::Events;
use crate::config::StoreConfig;
use crate::protocol::RunRequest;
use crate::status::{Disk, Evicted, LastGc, StoreUse, TargetUse};
use crate::store::ladder::{self, Policy};
use crate::store::registry::{Registry, Target};
use crate::store::sizes::Sizes;
use crate::store::{self, Kind, evict, orphan, owner, trash, volume};
use crate::{clock, config};

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
    events: Arc<Events>,
    log: Log,
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
}

struct Figures {
    kind: Kind,
    store: Option<PathBuf>,
    sizes: Sizes,
    scanned: Instant,
}

/// A workspace as cargo places it for a run.
#[derive(Debug, Deserialize)]
struct Workspace {
    #[serde(rename = "workspace_root")]
    root: PathBuf,
    #[serde(rename = "target_directory")]
    target: PathBuf,
    /// Absent from a cargo older than `build-dir`, whose build directory is
    /// the target directory.
    #[serde(rename = "build_directory")]
    build: Option<PathBuf>,
}

/// What it takes to ask about a run's workspace as the run would.
struct Probe {
    cwd: PathBuf,
    cargo: OsString,
    env: Vec<(OsString, OsString)>,
}

/// Kept while a run lasts; dropping it tells the accounts the run ended.
pub struct Learning {
    _ended: mpsc::Sender<()>,
}

impl Accounts {
    /// The accounts, with their accountant and the trash's deleter started;
    /// `config_file` holds the test seam.
    pub(super) fn start(
        config: StoreConfig,
        state_dir: PathBuf,
        config_file: PathBuf,
        events: Arc<Events>,
        log: Log,
    ) -> Arc<Self> {
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
            events,
            log,
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

    /// Learns, in the background, the workspace that run `request` builds
    /// in, if it is a run of cargo (see the module docs). Once the run has
    /// ended (the returned value is dropped), its directories are scanned
    /// again.
    pub fn learn(self: &Arc<Self>, request: &RunRequest) -> Option<Learning> {
        let probe = Probe::of(request)?;
        let at = clock::now();
        let (ended, end) = mpsc::channel();
        let accounts = Arc::clone(self);
        std::thread::spawn(move || {
            let Some(dirs) = accounts.discover(&probe, at) else {
                return;
            };
            // An error: the run has ended.
            let _ = end.recv();
            let mut ledger = accounts.ledger();
            ledger.dirty.extend(dirs);
            drop(ledger);
            accounts.wake.notify_all();
        });
        Some(Learning { _ended: ended })
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
        let targets = ledger
            .figures
            .iter()
            .filter(|(_, figures)| figures.kind == Kind::Target)
            .map(|(path, figures)| TargetUse {
                path: text(path),
                used: figures.sizes.total(),
                by_category: figures.sizes,
            })
            .collect();
        Disk { stores, targets }
    }

    /// Asks cargo where the workspace of `probe` builds, and records a run
    /// there that started `at`. Returns the directories the run builds in.
    fn discover(&self, probe: &Probe, at: u64) -> Option<Vec<PathBuf>> {
        let workspace = probe.workspace()?;
        let build = workspace.build.clone().unwrap_or(workspace.target.clone());
        if store::store_of(&self.config, &build).is_some() {
            let repository = probe.repository(&workspace.root);
            let recorded = owner::record_run(&build, &workspace.root, repository.as_deref(), at);
            if let Err(error) = recorded {
                self.log.write(&format!(
                    "cannot write the owner manifest of {}: {error}",
                    build.display()
                ));
            }
        }
        // Last, so that a registered target directory says that all of this
        // is done.
        self.register(Target {
            path: workspace.target.clone(),
            workspace: workspace.root.clone(),
        });
        Some(vec![workspace.target, build])
    }

    /// Registers `target`.
    fn register(&self, target: Target) {
        let _writing = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = Registry::add(&self.state_dir, target) {
            self.log
                .write(&format!("cannot register a target directory: {error}"));
        }
    }

    /// The accountant: surveys the store and scans what needs it, for ever.
    fn keep(&self) {
        loop {
            // Each run that ended is in `ended` once: after it ended, the
            // survey below sees every directory it built.
            let ended = std::mem::take(&mut self.ledger().dirty);
            let registry = Registry::load(&self.state_dir);
            let dirs = store::survey(&self.config, &registry);
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
            if ledger.dirty.is_empty() {
                let _waited = self
                    .wake
                    .wait_timeout(ledger, LOOK)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    /// The time now for the ladder: the test seam's clock, if it has one.
    fn now(&self) -> u64 {
        ladder::now(config::current_seam(&self.config_file).as_ref())
    }

    /// Whether the figures are over the budget, or a volume they are on has
    /// less free than the floor.
    fn over_limits(&self) -> bool {
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
    fn evict(&self, registry: &Registry, now: u64) {
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
    fn empty_trash(&self) {
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

    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Probe {
    /// The probe for run `request`, if its program is named `cargo`.
    fn of(request: &RunRequest) -> Option<Self> {
        let program = &request.argv.first()?.0;
        let named_cargo = Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo"));
        if !named_cargo {
            return None;
        }
        Some(Self {
            cwd: PathBuf::from(&request.cwd.0),
            cargo: program.clone(),
            env: request
                .env
                .iter()
                .map(|(name, value)| (name.0.clone(), value.0.clone()))
                .collect(),
        })
    }

    /// `program args` as the run would start it, its stdout captured.
    fn command(&self, program: &OsString, args: &[&str], dir: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    /// The workspace as `cargo metadata` describes it.
    fn workspace(&self) -> Option<Workspace> {
        let output = self
            .command(
                &self.cargo,
                &["metadata", "--format-version", "1", "--no-deps"],
                &self.cwd,
            )
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// The git common directory of the repository holding `dir`, if any.
    fn repository(&self, dir: &Path) -> Option<PathBuf> {
        let output = self
            .command(
                &OsString::from("git"),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                dir,
            )
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let common = Path::new(text.trim_end());
        Some(store::simplified(common))
    }
}
