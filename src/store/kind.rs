//! What each kind of directory is to the build store (docs/build-store.md): the
//! one place for every rule that depends on it, so that a new kind is a new
//! variant here, which the compiler then asks to be decided in each rule below,
//! and nowhere else.
//!
//! | Kind    | Busy while a cargo lock is held | Trash                   | Tool caches | Its own rungs (`super::ladder`)                |
//! |---------|---------------------------------|-------------------------|-------------|------------------------------------------------|
//! | build   | anywhere inside                 | `<store>/.trash`        | none        | 2 orphan, 5 whole directory, 7 of no known workspace |
//! | target  | in its profile directories      | `<target>/.heli-trash`  | configured  | 6 superseded intermediates, 4 tool caches      |
//! | scratch | anywhere inside                 | `.heli-trash` beside it | none        | 0 whole copy                                   |
//! | shared  | in its profile directories      | `<target>/.heli-trash`  | none        | none                                           |
//! | home    | in its package-cache locks      | `.heli-trash` beside it | none        | 2 orphan or unknown, 5 whole home              |
//! | crate   | anywhere inside                 | `<store>/.trash`        | none        | 8 entry no build directory links               |
//!
//! Every kind but shared, home and crate also has the ladder's incremental
//! rungs, 1 and 3. A shared `target/` directory, seen for more than one workspace
//! (a global `CARGO_TARGET_DIR` or `build.target-dir`), is not heli's to
//! manage: it has no rungs at all, and does not count toward the budget
//! (#65). An isolated Cargo home (`super::home`) holds no intermediates,
//! and is only ever evicted whole (#120).

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::ladder::{Measure, Policy, Surveyed, Tier, hours, measure, tool_caches_in};
use super::{Dir, State, superseded};
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
    /// An entry of the shared crates in a store (`crate::crates`): it
    /// counts only what no build directory links.
    Crate,
    /// An isolated Cargo home of a worktree (`super::home`).
    Home,
}

/// Where a cargo's lock makes a directory busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockScope {
    /// Anywhere inside it: the whole directory may be a victim.
    Anywhere,
    /// Only in its profile directories: a `target/` directory's nested tool
    /// caches protect their own victims at eviction, not unrelated ones.
    Profiles,
    /// In the package-cache locks at its root, which cargo holds for a
    /// build (`super::home::LOCKS`).
    PackageCache,
}

impl Kind {
    /// How `gc` and `status` name it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Target => "target",
            Self::Scratch => "scratch",
            Self::SharedTarget => "shared",
            Self::Crate => "crate",
            Self::Home => "home",
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
            Self::Build | Self::Scratch | Self::Crate => LockScope::Anywhere,
            Self::Target | Self::SharedTarget => LockScope::Profiles,
            Self::Home => LockScope::PackageCache,
        }
    }

    /// Whether directories of this kind have the ladder's incremental
    /// rungs, 1 and 3: all but a shared `target/` directory, which has no
    /// rungs, and a home, which holds no intermediates.
    pub fn incremental_rungs(self) -> bool {
        match self {
            Self::Build | Self::Target | Self::Scratch => true,
            Self::SharedTarget | Self::Home | Self::Crate => false,
        }
    }

    /// Whether `workspace`, the workspace of a directory of this kind,
    /// still exists: its `Cargo.toml`, since a removed worktree can leave
    /// files behind; for a home, which belongs to a worktree, the
    /// worktree's `.git` entry, or, outside any repository, its
    /// `Cargo.toml` (`super::home::worktree_exists`).
    pub fn workspace_exists(self, workspace: &Path) -> bool {
        match self {
            Self::Home => super::home::worktree_exists(workspace),
            Self::Build | Self::Target | Self::Scratch | Self::SharedTarget | Self::Crate => {
                workspace.join("Cargo.toml").is_file()
            }
        }
    }
}

impl Dir {
    /// Where an eviction renames its victims, on its volume.
    pub fn trash(&self) -> Option<PathBuf> {
        match self.kind {
            Kind::Build | Kind::Crate => self.store.as_ref().map(|store| store.join(STORE_TRASH)),
            Kind::Target | Kind::SharedTarget => Some(self.path.join(TARGET_TRASH)),
            Kind::Scratch => Some(super::scratch::trash()),
            // Beside it: a home is directly under the homes' base.
            Kind::Home => self.path.parent().map(|base| base.join(super::home::TRASH)),
        }
    }

    /// The name of an entry inside it that is not its content (its own
    /// trash), which measuring it skips.
    pub fn own_trash(&self) -> Option<&'static str> {
        match self.kind {
            Kind::Target | Kind::SharedTarget => Some(TARGET_TRASH),
            Kind::Build | Kind::Scratch | Kind::Crate | Kind::Home => None,
        }
    }

    /// The tool-cache patterns that apply inside it.
    pub fn tool_caches(&self, config: &StoreConfig) -> Vec<String> {
        match self.kind {
            Kind::Target => config.tool_cache_patterns(),
            Kind::Build | Kind::Scratch | Kind::SharedTarget | Kind::Crate | Kind::Home => {
                Vec::new()
            }
        }
    }
}

/// A victim the rungs of its kind allow: its tier, path, bytes, age and the
/// rule's reason.
pub type Rung = (Tier, PathBuf, u64, u64, String);

/// The victims the rungs particular to `surveyed`'s kind allow at `now`
/// (the incremental rungs, shared by every kind, are the ladder's own);
/// `idle` is how long it has been idle. Under the floor (`floor`, the time
/// now), an orphan no pass has noted yet is unclaimed since now.
pub fn rungs(
    surveyed: &Surveyed,
    policy: &Policy,
    tool_caches: &[String],
    now: u64,
    idle: u64,
    floor: Option<u64>,
) -> Vec<Rung> {
    let root = &surveyed.dir.path;
    let whole = |tier: Tier, age: u64, reason: String| {
        vec![(tier, root.clone(), surveyed.measure.bytes, age, reason)]
    };
    match (surveyed.dir.kind, surveyed.state) {
        (Kind::Build, State::Orphan) => {
            let Some(since) = surveyed.orphaned_since.or(floor) else {
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
        (Kind::Build, State::Live) => {
            if idle < policy.dir_age {
                return Vec::new();
            }
            whole(
                Tier::BuildDir,
                idle,
                format!("build directory idle for {}", hours(idle)),
            )
        }
        (Kind::Build, State::Unknown) => {
            if idle < policy.unknown_age || !super::managed::unknown_is_reclaimable(&surveyed.dir) {
                return Vec::new();
            }
            whole(
                Tier::Unknown,
                idle,
                format!(
                    "build directory of no known workspace, untouched for {}",
                    hours(idle)
                ),
            )
        }
        (Kind::Build, State::Busy) => Vec::new(),
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
        (Kind::Target, _) => {
            let mut rungs: Vec<Rung> = surveyed
                .dir
                .separation
                .iter()
                .flat_map(|separation| {
                    superseded::find(root, separation, tool_caches)
                        .into_iter()
                        .map(|(dir, held)| {
                            (
                                Tier::Superseded,
                                dir,
                                held.bytes,
                                now.saturating_sub(separation.since),
                                format!(
                                    "intermediates superseded by build-dir {}",
                                    separation.build_dir.display()
                                ),
                            )
                        })
                })
                .collect();
            if idle >= policy.idle_age {
                rungs.extend(tool_caches_in(root, tool_caches).into_iter().map(
                    |(cache, pattern)| {
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
                    },
                ));
            }
            rungs
        }
        (Kind::SharedTarget, _) => Vec::new(),
        (Kind::Crate, State::Unknown) => {
            if idle < policy.crate_age {
                return Vec::new();
            }
            whole(
                Tier::Crate,
                idle,
                format!(
                    "shared crate no build directory links, unused for {}",
                    hours(idle)
                ),
            )
        }
        (Kind::Crate, _) => Vec::new(),
        // A home of a removed worktree waits out `orphan_hours` like an
        // orphan build directory, so a worktree that is being moved or
        // repaired keeps it; nothing claims it otherwise (#120).
        (Kind::Home, State::Orphan) => {
            let Some(since) = surveyed.orphaned_since.or(floor) else {
                return Vec::new();
            };
            let unclaimed = now.saturating_sub(since);
            if unclaimed < policy.orphan_age {
                return Vec::new();
            }
            whole(
                Tier::Orphan,
                unclaimed,
                format!(
                    "Cargo home of a removed worktree, unclaimed for {}",
                    hours(unclaimed)
                ),
            )
        }
        // Nothing heli knows selects a home without an owner manifest (one
        // an older heli published): an orphan too, once idle that long.
        (Kind::Home, State::Unknown) => {
            if idle < policy.orphan_age {
                return Vec::new();
            }
            whole(
                Tier::Orphan,
                idle,
                format!("Cargo home of no known worktree, idle for {}", hours(idle)),
            )
        }
        (Kind::Home, State::Live) => {
            if idle < policy.dir_age {
                return Vec::new();
            }
            whole(
                Tier::BuildDir,
                idle,
                format!("Cargo home idle for {}", hours(idle)),
            )
        }
        (Kind::Home, State::Busy) => Vec::new(),
    }
}

/// Why a directory is not deleted whole ([`kept`]): the rule, for people,
/// and the group `gc` counts it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub group: &'static str,
    pub why: String,
}

/// Why `surveyed`, idle for `idle` at `now`, is not deleted whole by the
/// rungs of its kind: the rule that keeps it. `None` when they allow it
/// whole (the plan may still leave it: over an unmet budget, or once the
/// goals are met).
pub fn kept(surveyed: &Surveyed, policy: &Policy, now: u64, idle: u64) -> Option<Kept> {
    let kept = |group: &'static str, why: String| Some(Kept { group, why });
    let of = |age: u64, limit: u64, setting: &str| {
        format!("{} of {} ([store] {setting})", hours(age), hours(limit))
    };
    match (surveyed.dir.kind, surveyed.state) {
        (_, State::Busy) => kept("busy", "busy: a cargo builds in it".to_owned()),
        (Kind::Build, State::Orphan) => {
            let claimable = surveyed
                .dir
                .owner
                .as_ref()
                .is_none_or(|owner| owner.repository.is_some());
            if !claimable {
                return None;
            }
            let Some(since) = surveyed.orphaned_since else {
                return kept(
                    "orphan within orphan_hours",
                    "orphan no pass has noted yet: unclaimed from the next pass on".to_owned(),
                );
            };
            let unclaimed = now.saturating_sub(since);
            (unclaimed < policy.orphan_age).then(|| Kept {
                group: "orphan within orphan_hours",
                why: format!(
                    "orphan unclaimed for {}: a worktree of its repository may claim it",
                    of(unclaimed, policy.orphan_age, "orphan_hours")
                ),
            })
        }
        (Kind::Build, State::Live) => (idle < policy.dir_age).then(|| Kept {
            group: "live within dir_hours",
            why: format!("live, idle for {}", of(idle, policy.dir_age, "dir_hours")),
        }),
        (Kind::Build, State::Unknown) => {
            if !super::managed::unknown_is_reclaimable(&surveyed.dir) {
                return kept(
                    "no known workspace, not cargo's layout",
                    "no known workspace, and not of cargo's layout: never deleted whole".to_owned(),
                );
            }
            (idle < policy.unknown_age).then(|| Kept {
                group: "no known workspace within unknown_hours",
                why: format!(
                    "no known workspace, untouched for {}",
                    of(idle, policy.unknown_age, "unknown_hours")
                ),
            })
        }
        (Kind::Scratch, _) => {
            let untouched = now.saturating_sub(surveyed.measure.newest.unwrap_or(0));
            (untouched < policy.scratch_age).then(|| Kept {
                group: "scratch within scratch_hours",
                why: format!(
                    "scratch copy untouched for {}",
                    of(untouched, policy.scratch_age, "scratch_hours")
                ),
            })
        }
        (Kind::Target, _) => kept(
            "target/ in its worktree",
            "target/ in its worktree: never deleted whole".to_owned(),
        ),
        (Kind::SharedTarget, _) => kept(
            "shared",
            "shared by workspaces: not heli's to manage".to_owned(),
        ),
        (Kind::Crate, State::Live) => kept(
            "shared crates linked into build directories",
            "linked into build directories: it goes with them".to_owned(),
        ),
        (Kind::Crate, _) => (idle < policy.crate_age).then(|| Kept {
            group: "shared crates within crate_hours",
            why: format!(
                "shared crate unused for {}",
                of(idle, policy.crate_age, "crate_hours")
            ),
        }),
        (Kind::Home, State::Orphan) => {
            let Some(since) = surveyed.orphaned_since else {
                return kept(
                    "orphan within orphan_hours",
                    "Cargo home of a removed worktree no pass has noted yet".to_owned(),
                );
            };
            let unclaimed = now.saturating_sub(since);
            (unclaimed < policy.orphan_age).then(|| Kept {
                group: "orphan within orphan_hours",
                why: format!(
                    "Cargo home of a removed worktree, unclaimed for {}",
                    of(unclaimed, policy.orphan_age, "orphan_hours")
                ),
            })
        }
        (Kind::Home, State::Unknown) => (idle < policy.orphan_age).then(|| Kept {
            group: "orphan within orphan_hours",
            why: format!(
                "Cargo home of no known worktree, idle for {}",
                of(idle, policy.orphan_age, "orphan_hours")
            ),
        }),
        (Kind::Home, State::Live) => (idle < policy.dir_age).then(|| Kept {
            group: "live within dir_hours",
            why: format!(
                "live Cargo home, idle for {}",
                of(idle, policy.dir_age, "dir_hours")
            ),
        }),
    }
}
