//! What each kind of directory is to the build store (DESIGN.md §6): the one
//! place for every rule that depends on it, so that a new kind is a new
//! variant here, which the compiler then asks to be decided in each rule
//! below, and nowhere else.
//!
//! | Kind    | Busy while a cargo lock is held | Trash                   | Tool caches | Its own rungs (`super::ladder`)                |
//! |---------|---------------------------------|-------------------------|-------------|------------------------------------------------|
//! | build   | anywhere inside                 | `<store>/.trash`        | none        | 2 orphan, 5 whole directory                    |
//! | target  | in its profile directories      | `<target>/.heli-trash`  | configured  | 4 tool caches                                  |
//! | scratch | anywhere inside                 | `.heli-trash` beside it | none        | 0 whole copy                                   |
//! | shared  | in its profile directories      | `<target>/.heli-trash`  | none        | none                                           |
//!
//! Every kind but shared also has the ladder's incremental rungs, 1 and 3.
//! A shared `target/` directory, seen for more than one workspace (a global
//! `CARGO_TARGET_DIR` or `build.target-dir`), is not heli's to manage: it
//! has no rungs at all, and does not count toward the budget (#65).

use std::path::PathBuf;

use serde::Serialize;

use super::ladder::{Measure, Policy, Surveyed, Tier, hours, measure, tool_caches_in};
use super::{Dir, State};
use crate::config::StoreConfig;

/// A store's trash, in the store.
pub const STORE_TRASH: &str = ".trash";

/// A registered `target/` directory's trash, in it.
pub const TARGET_TRASH: &str = ".heli-trash";

/// What a directory is to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A build directory in a store.
    Build,
    /// A registered `target/` directory.
    Target,
    /// A scratch copy a cargo tool left in the temporary directory
    /// (`super::scratch`).
    Scratch,
    /// A registered `target/` directory seen for more than one workspace
    /// (`super::registry::Target::shared`).
    #[serde(rename = "shared")]
    SharedTarget,
}

/// Where a cargo's lock makes a directory busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockScope {
    /// Anywhere inside it: the whole directory may be a victim.
    Anywhere,
    /// Only in its profile directories: a `target/` directory's nested tool
    /// caches protect their own victims at eviction, not unrelated ones.
    Profiles,
}

impl Kind {
    /// How `gc` and `status` name it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Target => "target",
            Self::Scratch => "scratch",
            Self::SharedTarget => "shared",
        }
    }

    /// Whether heli manages directories of this kind: evicts from them and
    /// counts them toward the budget. Not a shared `target/` directory.
    pub fn managed(self) -> bool {
        !matches!(self, Self::SharedTarget)
    }

    /// Where a cargo's lock makes a directory of this kind busy.
    pub fn lock_scope(self) -> LockScope {
        match self {
            Self::Build | Self::Scratch => LockScope::Anywhere,
            Self::Target | Self::SharedTarget => LockScope::Profiles,
        }
    }
}

impl Dir {
    /// Where an eviction renames its victims, on its volume.
    pub fn trash(&self) -> Option<PathBuf> {
        match self.kind {
            Kind::Build => self.store.as_ref().map(|store| store.join(STORE_TRASH)),
            Kind::Target | Kind::SharedTarget => Some(self.path.join(TARGET_TRASH)),
            Kind::Scratch => Some(super::scratch::trash()),
        }
    }

    /// The name of an entry inside it that is not its content (its own
    /// trash), which measuring it skips.
    pub fn own_trash(&self) -> Option<&'static str> {
        match self.kind {
            Kind::Target | Kind::SharedTarget => Some(TARGET_TRASH),
            Kind::Build | Kind::Scratch => None,
        }
    }

    /// The tool-cache patterns that apply inside it.
    pub fn tool_caches(&self, config: &StoreConfig) -> Vec<String> {
        match self.kind {
            Kind::Target => config.tool_cache_patterns(),
            Kind::Build | Kind::Scratch | Kind::SharedTarget => Vec::new(),
        }
    }
}

/// A victim the rungs of its kind allow: its tier, path, bytes, age and the
/// rule's reason.
pub type Rung = (Tier, PathBuf, u64, u64, String);

/// The victims the rungs particular to `surveyed`'s kind allow at `now`
/// (the incremental rungs, shared by every kind, are the ladder's own);
/// `idle` is how long it has been idle.
pub fn rungs(
    surveyed: &Surveyed,
    policy: &Policy,
    tool_caches: &[String],
    now: u64,
    idle: u64,
) -> Vec<Rung> {
    let root = &surveyed.dir.path;
    let whole = |tier: Tier, age: u64, reason: String| {
        vec![(tier, root.clone(), surveyed.measure.bytes, age, reason)]
    };
    match (surveyed.dir.kind, surveyed.state) {
        (Kind::Build, State::Orphan) => {
            let Some(since) = surveyed.orphaned_since else {
                return Vec::new();
            };
            let unclaimed = now.saturating_sub(since);
            // Only a worktree of its repository can claim it (recycling):
            // without one, nothing will, and it need not wait (#88).
            let claimable = surveyed
                .dir
                .owner
                .as_ref()
                .is_none_or(|owner| owner.repository.is_some());
            if claimable && unclaimed < policy.orphan_age {
                return Vec::new();
            }
            whole(
                Tier::Orphan,
                unclaimed,
                format!("orphan unclaimed for {}", hours(unclaimed)),
            )
        }
        (Kind::Build, state)
            if state == State::Live
                || (state == State::Unknown
                    && super::managed::unknown_is_reclaimable(&surveyed.dir)) =>
        {
            if idle < policy.dir_age {
                return Vec::new();
            }
            whole(
                Tier::BuildDir,
                idle,
                format!("build directory idle for {}", hours(idle)),
            )
        }
        (Kind::Build, _) => Vec::new(),
        (Kind::Scratch, _) => {
            let untouched = now.saturating_sub(surveyed.measure.newest.unwrap_or(0));
            if untouched < policy.scratch_age {
                return Vec::new();
            }
            whole(
                Tier::Scratch,
                untouched,
                format!(
                    "scratch copy left by a cargo tool, untouched for {}",
                    hours(untouched)
                ),
            )
        }
        (Kind::Target, _) if idle >= policy.idle_age => tool_caches_in(root, tool_caches)
            .into_iter()
            .map(|(cache, pattern)| {
                let Measure { bytes, .. } = measure(&cache);
                (
                    Tier::ToolCache,
                    cache,
                    bytes,
                    idle,
                    format!(
                        "tool cache {pattern} of a target/ directory idle for {}",
                        hours(idle)
                    ),
                )
            })
            .collect(),
        (Kind::Target, _) | (Kind::SharedTarget, _) => Vec::new(),
    }
}
