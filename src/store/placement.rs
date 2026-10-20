//! Placement under the floor (docs/build-store.md#placement, #135): a run
//! whose build directory would land in a store on a volume under its free
//! floor builds in the first configured store whose volume is above it.
//!
//! cargo names the build directory from `build.build-dir`, which setup
//! points at the store on the worktree's own volume, so that cargo
//! hardlinks final artifacts out of it. Once that volume is under
//! `[store] floor_mb`, the next build there fails for lack of space, which
//! costs more than the copies cargo makes across volumes. So a managed
//! run gets `CARGO_BUILD_BUILD_DIR=<other store>/{workspace-path-hash}`,
//! which its nested cargos inherit too, and its clients a note.
//!
//! It costs nothing while every store's volume is above the floor: only
//! then does heli ask cargo where the build directory is
//! (`cargo metadata --no-deps`). A `build-dir` of the user's own (outside
//! every store heli manages) is never moved.

use std::path::{Path, PathBuf};

use crate::config::StoreConfig;

/// A megabyte, as the configuration counts.
const MB: u64 = 1 << 20;

/// The volumes as [`redirect`] sees them: the root of a path's volume,
/// and a volume's free bytes.
pub struct Volumes<R, F> {
    pub root: R,
    pub free: F,
}

/// Where a run in `cwd` builds instead, as the value of
/// `CARGO_BUILD_BUILD_DIR`, with a note for its clients; `None` when its
/// build directory stays where cargo puts it. `build_dir` says where cargo
/// would build.
pub fn redirect<R, F>(
    config: &StoreConfig,
    cwd: &Path,
    volumes: &Volumes<R, F>,
    build_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<(PathBuf, String)>
where
    R: Fn(&Path) -> Option<PathBuf>,
    F: Fn(&Path) -> Option<u64>,
{
    let floor = config.floor_mb?.saturating_mul(MB);
    let free = |store: &Path| (volumes.root)(store).and_then(|root| (volumes.free)(&root));
    let full = |store: &Path| free(store).is_some_and(|free| free < floor);
    // Stores that may receive the build directory: configured, on a volume
    // above the floor, in the configuration's order.
    let open: Vec<&PathBuf> = config
        .paths
        .iter()
        .filter(|store| free(store).is_some_and(|free| free >= floor))
        .collect();
    if open.is_empty() {
        return None;
    }
    let mut candidates = config
        .paths
        .iter()
        .cloned()
        .chain(super::serving(config, cwd).ok());
    if !candidates.any(|store| full(&store)) {
        return None;
    }
    let build_dir = build_dir()?;
    let store = super::store_of(config, &build_dir)?;
    if !full(&store) {
        return None;
    }
    let volume = (volumes.root)(&store)?;
    let to = open
        .into_iter()
        .find(|open| (volumes.root)(open).is_some_and(|root| root != volume))?;
    let note = format!(
        "cargo-heli: note: the volume of {} is under its floor ([store] floor_mb); \
         building in {} instead",
        store.display(),
        to.display()
    );
    Some((super::build_dir(to), note))
}

/// What [`redirect`] says for a run in `cwd`, asked of this host's volumes
/// and cargo.
pub fn here(config: &StoreConfig, cwd: &Path) -> Option<(PathBuf, String)> {
    let volumes = Volumes {
        root: super::volume::root,
        free: super::volume::free,
    };
    redirect(config, cwd, &volumes, || {
        if std::env::var_os("CARGO_BUILD_BUILD_DIR").is_some_and(|dir| !dir.is_empty()) {
            // An inherited choice, a parent run's or the user's: kept.
            return None;
        }
        super::probe::Probe::here(cwd.to_path_buf())
            .workspace()
            .map(|workspace| workspace.build_dir())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(paths: &[&Path], floor_mb: u64) -> StoreConfig {
        StoreConfig {
            paths: paths.iter().map(|path| path.to_path_buf()).collect(),
            floor_mb: Some(floor_mb),
            ..StoreConfig::default()
        }
    }

    /// Stores `e/` and `d/` stand for two volumes: a path's volume is its
    /// first component under the temporary directory.
    #[test]
    fn a_full_store_sends_the_build_to_the_first_open_one_elsewhere() {
        let dir = crate::test_tmp::tempdir();
        let base = dir.path().to_path_buf();
        let e = base.join("e").join("heli-cache");
        let d = base.join("d").join("heli-cache");
        let d2 = base.join("d").join("other-cache");
        let build = d.join("ab").join("0123456789abcd");
        let worktree = base.join("d").join("flui");
        let root = |path: &Path| {
            let below = path.strip_prefix(&base).ok()?;
            Some(base.join(below.components().next()?))
        };
        let full_d = Volumes {
            root,
            free: |volume: &Path| Some(if volume.ends_with("d") { 0 } else { 100 << 20 }),
        };
        // D: under its floor, E: above: the build goes to E:.
        let config = config(&[&d2, &e, &d], 10);
        let (to, note) =
            redirect(&config, &worktree, &full_d, || Some(build.clone())).expect("redirected");
        assert_eq!(to, e.join(crate::store::WORKSPACE_PATH_HASH));
        assert!(note.contains("under its floor"), "{note}");
        // Nothing is asked of cargo while every volume is above the floor.
        let roomy = Volumes {
            root,
            free: |_: &Path| Some(100 << 20),
        };
        assert_eq!(
            redirect(&config, &worktree, &roomy, || panic!("cargo asked")),
            None
        );
        // Nor without a floor.
        let no_floor = StoreConfig {
            floor_mb: None,
            ..config.clone()
        };
        assert_eq!(
            redirect(&no_floor, &worktree, &full_d, || panic!("cargo asked")),
            None
        );
        // A build directory outside every store heli manages stays.
        assert_eq!(
            redirect(&config, &worktree, &full_d, || Some(
                base.join("d").join("mine")
            )),
            None
        );
        // With every volume full, there is nowhere to go.
        let all_full = Volumes {
            root,
            free: |_: &Path| Some(0),
        };
        assert_eq!(
            redirect(&config, &worktree, &all_full, || Some(build.clone())),
            None
        );
    }
}
