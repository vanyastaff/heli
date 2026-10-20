//! **The look** (`Accounts::look`). Each pass of the accountant first
//! lists the worktrees of every repository with an orphan in a store
//! (`git worktree list`); each linked worktree it has not looked at before
//! gets a build directory recycled if it has none
//! (`crate::store::recycle::for_worktree`): one added without the
//! `post-checkout` hook, or before the daemon started.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{Accounts, Kind, State};
use crate::clock;
use crate::store;
use crate::store::probe::Probe;
use crate::store::recycle::{self, Recycled};
use crate::store::registry::Registry;

impl Accounts {
    /// Recycles an orphan into `build`, the build directory of `workspace`
    /// in `repository`, if it has none, asking git through `probe`; logs it
    /// as set off by `trigger`.
    pub(super) fn recycle(
        &self,
        build: &Path,
        workspace: &Path,
        repository: &Path,
        at: u64,
        trigger: &str,
        probe: &Probe,
    ) -> Option<Recycled> {
        match recycle::claim(&self.config, build, workspace, repository, at, probe) {
            Ok(Some(recycled)) => {
                self.recycled(&recycled, trigger);
                Some(recycled)
            }
            Ok(None) => None,
            Err(error) => {
                self.log.write(&format!(
                    "cannot recycle a build directory for {}: {error}",
                    workspace.display()
                ));
                None
            }
        }
    }

    /// Logs `recycled`, set off by `trigger`: in the daemon's log, and as a
    /// `recycle` event.
    pub(super) fn recycled(&self, recycled: &Recycled, trigger: &str) {
        self.log.write(&format!(
            "{} ({}): {}",
            recycled.workspace.display(),
            recycled.to.display(),
            recycled.describe()
        ));
        self.events.record("recycle", recycled.event(trigger));
    }

    /// The look at worktrees: each linked worktree of a repository that has
    /// an orphan in a store, which this look has not seen before, gets a
    /// build directory recycled if it has none (`recycle::for_worktree`).
    /// The main worktree is never new, and is left to its first run.
    pub(super) fn look(&self, registry: &Registry, looked: &mut BTreeSet<PathBuf>) {
        let mut repositories: Vec<PathBuf> = Vec::new();
        for dir in store::survey(&self.config, registry, &self.state_dir) {
            let Some(repository) = dir
                .owner
                .as_ref()
                .and_then(|owner| owner.repository.clone())
            else {
                continue;
            };
            let known = repositories
                .iter()
                .any(|known| store::same_path(known, &repository));
            if !known && dir.kind == Kind::Build && dir.state() == State::Orphan {
                repositories.push(repository);
            }
        }
        let lister = Probe::here(self.state_dir.clone());
        for repository in repositories {
            for worktree in lister.worktrees(&repository).into_iter().skip(1) {
                if !looked.insert(worktree.clone()) {
                    continue;
                }
                let probe = Probe::here(worktree.clone());
                let at = clock::now();
                match recycle::for_worktree(&self.config, registry, &probe, &worktree, at) {
                    Ok(Some(recycled)) => self.recycled(&recycled, "look"),
                    Ok(None) => {}
                    Err(error) => self.log.write(&format!(
                        "cannot recycle a build directory for {}: {error}",
                        worktree.display()
                    )),
                }
            }
        }
    }
}
