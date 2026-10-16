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
//! and the repository git names for the workspace. Nothing of this can
//! fail the run: what goes wrong goes to the daemon's log.
//!
//! **Recycling** (DESIGN.md §6.4). When the build directory cargo names is
//! in a store heli manages and does not exist yet, the run's worktree gets
//! the most recently used orphan of its repository, renamed into place
//! (`crate::store::recycle`), before the owner manifest is written. That
//! must happen before the run's cargo creates the build directory, so the
//! first run from a directory waits for its learning
//! ([`Learning::prepared`], at most [`PREPARE_WAIT`]) while a store holds
//! an orphan of its own repository, which it could get; no other run waits
//! for it. Each
//! recycling is a `recycle` event in the event log, with what it moved and
//! how many units the path scan invalidated, and a note to the run's
//! clients.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::{Accounts, Kind, State};
use crate::protocol::RunRequest;
use crate::store::probe::Probe;
use crate::store::recycle::Recycled;
use crate::store::registry::{Registry, Target};
use crate::store::{self, owner};
use crate::{clock, worktree};

/// The longest a run waits for its build directory to be recycled.
pub const PREPARE_WAIT: Duration = Duration::from_secs(30);

/// The longest a run's end waits for the source-state record of its build
/// directory, which is written anyway.
pub(super) const RECORD_WAIT: Duration = Duration::from_secs(10);

/// Kept while a run lasts; [`Learning::ended`], or dropping it, tells the
/// accounts the run ended.
pub struct Learning {
    ended: mpsc::Sender<()>,
    /// Hears what was recycled for the run, once that is settled; `None`
    /// when the run need not wait for it.
    ready: Option<Mutex<mpsc::Receiver<Option<Recycled>>>>,
    /// Closed once the run's source-state record is written, or will not
    /// be.
    recorded: Mutex<mpsc::Receiver<()>>,
}

impl Learning {
    /// Waits, at most [`PREPARE_WAIT`], until the run's build directory is
    /// recycled or will not be; returns what was recycled for it.
    pub fn prepared(&self) -> Option<Recycled> {
        let ready = self
            .ready
            .as_ref()?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ready.recv_timeout(PREPARE_WAIT).ok().flatten()
    }

    /// Tells the accounts the run ended, and waits, at most
    /// [`RECORD_WAIT`], until the source-state record of its build directory
    /// is written, or will not be.
    pub fn ended(&self) {
        let _ = self.ended.send(());
        let recorded = self.recorded.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = recorded.recv_timeout(RECORD_WAIT);
    }
}

/// Where a run builds, as its learning found it.
pub(super) struct Learnt {
    /// The directories to scan again once it has ended.
    dirs: Vec<PathBuf>,
    /// Its build directory and the worktree of its workspace, if the build
    /// directory is in a store heli manages and the worktree is in a
    /// repository: what a source-state record is written for.
    recorded: Option<(PathBuf, PathBuf)>,
}

impl Accounts {
    /// Learns, in the background, the workspace that run `request` builds
    /// in, if it is a run of cargo (see the module docs), and recycles a
    /// build directory for it if it has none. Once the run has ended (the
    /// returned value is dropped), its directories are scanned again.
    pub fn learn(
        self: &Arc<Self>,
        request: &RunRequest,
        epoch_lease_root: Option<&Path>,
    ) -> Option<Learning> {
        let probe = Probe::of(request)?;
        // Discovery can outlive the logical run: ended() waits only a
        // bounded time. Own immutable inputs in the actual consumer thread,
        // not merely in the daemon handler which launched it.
        let epoch_guard = match epoch_lease_root.map(store::lease::shared).transpose() {
            Ok(guard) => guard,
            Err(error) => {
                self.log
                    .write(&format!("cannot protect dependency discovery: {error}"));
                return None;
            }
        };
        let at = clock::now();
        let (ended, end) = mpsc::channel();
        let (prepared, ready) = mpsc::channel();
        let (recorded, written) = mpsc::channel::<()>();
        let wait = self.may_recycle(&probe);
        let accounts = Arc::clone(self);
        std::thread::spawn(move || {
            let _epoch_guard = epoch_guard;
            let Some(learnt) = accounts.discover(&probe, at, prepared) else {
                return;
            };
            if let Some((build, _)) = &learnt.recorded {
                accounts.recording().push(build.clone());
            }
            // An error: the run has ended too.
            let _ = end.recv();
            if let Some((build, worktree)) = &learnt.recorded {
                accounts.record(build, worktree, &probe);
                let mut recording = accounts.recording();
                if let Some(index) = recording.iter().position(|dir| dir == build) {
                    recording.swap_remove(index);
                }
            }
            drop(recorded);
            let mut ledger = accounts.ledger();
            ledger.dirty.extend(learnt.dirs);
            drop(ledger);
            accounts.wake.notify_all();
        });
        Some(Learning {
            ended,
            ready: wait.then(|| Mutex::new(ready)),
            recorded: Mutex::new(written),
        })
    }

    /// Whether the first run of `probe` must wait for its learning: no run
    /// from its directory was learnt yet, and a store holds an orphan of its
    /// own repository, which its worktree may get. git is asked for the
    /// repository only while a store holds an orphan of any.
    pub(super) fn may_recycle(&self, probe: &Probe) -> bool {
        let prepared = self.prepared.lock().unwrap_or_else(PoisonError::into_inner);
        if prepared.contains(&probe.cwd) {
            return false;
        }
        drop(prepared);
        let registry = Registry::load(&self.state_dir);
        let repositories: Vec<PathBuf> = store::survey(&self.config, &registry)
            .into_iter()
            .filter(|dir| dir.kind == Kind::Build && dir.state() == State::Orphan)
            .filter_map(|dir| dir.owner?.repository)
            .collect();
        if repositories.is_empty() {
            return false;
        }
        probe.repository(&probe.cwd).is_some_and(|ours| {
            repositories
                .iter()
                .any(|theirs| store::same_path(theirs, &ours))
        })
    }

    /// Asks cargo where the workspace of `probe` builds, recycles a build
    /// directory for it if it has none (and tells `prepared` what, or that
    /// nothing was), and records a run there that started `at`. Returns
    /// where the run builds.
    pub(super) fn discover(
        &self,
        probe: &Probe,
        at: u64,
        prepared: mpsc::Sender<Option<Recycled>>,
    ) -> Option<Learnt> {
        let workspace = probe.workspace()?;
        let build = workspace.build_dir();
        let mut recorded = None;
        if store::store_of(&self.config, &build).is_some() {
            let repository = probe.repository(&workspace.root);
            let recycled = repository.as_deref().and_then(|repository| {
                self.recycle(&build, &workspace.root, repository, at, "run", probe)
            });
            self.prepared(&probe.cwd);
            let _ = prepared.send(recycled);
            let written = owner::record_run(&build, &workspace.root, repository.as_deref(), at);
            if let Err(error) = written {
                self.log.write(&format!(
                    "cannot write the owner manifest of {}: {error}",
                    build.display()
                ));
            }
            if repository.is_some() {
                recorded = Some((build.clone(), worktree::of(&workspace.root)));
            }
        } else {
            self.prepared(&probe.cwd);
        }
        // Last, so that a registered target directory says that all of this
        // is done.
        self.register(Target {
            path: workspace.target.clone(),
            workspace: workspace.root.clone(),
        });
        Some(Learnt {
            dirs: vec![workspace.target, build],
            recorded,
        })
    }

    /// Notes that runs from `cwd` need not wait for their learning.
    pub(super) fn prepared(&self, cwd: &Path) {
        self.prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(cwd.to_path_buf());
    }

    /// Registers `target`.
    pub(super) fn register(&self, target: Target) {
        let _writing = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = Registry::add(&self.state_dir, target) {
            self.log
                .write(&format!("cannot register a target directory: {error}"));
        }
    }
}
