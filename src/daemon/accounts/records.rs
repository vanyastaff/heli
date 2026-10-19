//! **Source-state records** (docs/build-store.md#recycling, "Precise";
//! `crate::store::record`), which make the next recycling of a build
//! directory precise. A run's build directory in a store heli manages, in a
//! repository, gets one as the run ends ([`super::Learning::ended`], which
//! the run waits for, at most [`super::learning::RECORD_WAIT`], before its
//! clients hear its end). And
//! each pass of the accountant writes one for each build directory in which
//! one of cargo's locks was held at the pass before and none is now
//! (`Accounts::released`): a build without heli, which the accountant saw.
//! It leaves alone a build directory where a run in flight will write one
//! as it ends, and one whose record still counts, so that the worktree is
//! not read again just after a run. None is written while one of cargo's
//! locks there is held.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{Accounts, Dir, Kind};
use crate::store::probe::Probe;
use crate::store::{self, record};
use crate::worktree;

impl Accounts {
    /// Writes the source-state record of `build`, the build directory of a
    /// workspace in `worktree`, asking git through `probe`; unless a build
    /// holds one of cargo's locks there.
    pub(super) fn record(&self, build: &Path, worktree: &Path, probe: &Probe) {
        if let Err(error) = record::write(build, worktree, probe) {
            self.log.write(&format!(
                "cannot write the source-state record of {}: {error}",
                build.display()
            ));
        }
    }

    /// Writes the source-state record of each build directory among `dirs`
    /// in which one of cargo's locks was held at the last look (`held`) and
    /// none is now, if its workspace exists in a repository. Not where a run
    /// in flight writes one as it ends, nor where the record still counts:
    /// nothing was built since. Then `held` becomes those whose locks are
    /// held now.
    pub(super) fn released(&self, dirs: &[Dir], held: &mut BTreeSet<PathBuf>) {
        let mut now = BTreeSet::new();
        for dir in dirs.iter().filter(|dir| dir.kind == Kind::Build) {
            if store::busy(&dir.path) {
                now.insert(dir.path.clone());
                continue;
            }
            let Some(owner) = dir.owner.as_ref().filter(|_| held.contains(&dir.path)) else {
                continue;
            };
            let recording = self
                .recording()
                .iter()
                .any(|build| store::same_path(build, &dir.path));
            if recording || record::valid(&dir.path).is_some() {
                continue;
            }
            if owner.repository.is_some() && owner.workspace.join("Cargo.toml").is_file() {
                let worktree = worktree::of(&owner.workspace);
                let probe = Probe::here(worktree.clone());
                self.record(&dir.path, &worktree, &probe);
            }
        }
        *held = now;
    }
}
