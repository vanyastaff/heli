//! The eviction ladder (docs/build-store.md#eviction): what heli evicts when
//! the build store is over its budget B or a volume is under its free-space
//! floor F, cheapest to regenerate first.
//!
//! **When.** Over budget (every build directory and registered `target/`
//! directory together take more than B), heli evicts until they take at
//! most 85 % of B. Under the floor (a volume holding a store or a
//! registered `target/` directory has less than F free, counting what its
//! trash still holds as free), it evicts on that volume until F is free.
//! Without a budget or a floor, nothing is evicted.
//!
//! **What, in this order** (a tier's ages are configurable, `[store]`):
//!
//! 6. superseded intermediates in registered `target/` directories whose
//!    workspace builds elsewhere (`super::superseded`), whatever their age:
//!    tier 6 is the newest rung, and the cheapest, so it comes first;
//! 0. scratch copies cargo tools left in the temporary directory
//!    (`super::scratch`) untouched for 1 h (`scratch_hours`), oldest first;
//! 7. build directories of no known workspace (`unknown`: bare or nested
//!    cargo built in them, and no heli run) untouched for 1 h
//!    (`unknown_hours`), oldest first: what tests that run cargo in
//!    temporary workspaces leave behind;
//! 1. `incremental/<unit>` directories untouched for 24 h (`unit_hours`),
//!    oldest first across every directory;
//! 2. orphans no worktree claimed for 24 h (`orphan_hours`), longest
//!    unclaimed first (`super::orphan`);
//! 3. all of a profile's `incremental/` in directories idle for 6 h
//!    (`idle_hours`), longest idle first;
//! 4. the tool caches (`[store] tool_caches`, and the built-in
//!    `llvm-cov-target`) of registered `target/`
//!    directories idle for 6 h, longest idle first;
//! 5. whole build directories of live workspaces idle for 7 days
//!    (`dir_hours`, 168), longest idle first.
//!
//! A victim inside one taken before only counts the bytes left in it; one
//! inside a victim taken before is not one.
//!
//! **Which directories.** A busy directory has no victims. An orphan
//! without a repository, which no worktree can claim, does not wait out
//! `orphan_hours` (#88). A build directory without an owner manifest that
//! `super::attribute` cannot attribute (`unknown`: only bare or nested
//! cargo built in, and compiled no object file) has tiers 1, 3 and 7, the
//! last when it is of cargo's layout (`super::managed`). It never counts
//! as an orphan. A registered `target/` directory has tiers 6, 1, 3 and 4,
//! and is never deleted whole: it is in the user's worktree.
//!
//! **Under the floor.** A volume with less than F free really is full:
//! the next build on it fails for lack of space, which costs more than any
//! rebuild. So there the ages do not hold. Once the candidates above are
//! spent, [`urgent`] takes, on that volume only, what no live workspace
//! will miss (scratch copies, build directories of no known workspace, and
//! orphans whatever `orphan_hours`), longest unused first; then the live
//! directories' units, `incremental/` and tool caches; and last the live
//! directories whole, longest idle first. Such a victim serves the floor,
//! never the budget ([`Victim::floor`], #134).
//!
//! **Ages.** A unit is untouched since its newest file was written (a
//! directory without files: since it was). A directory is idle since its
//! newest file was written, or its last heli run started (`super::owner`),
//! whichever is later. Times are whole seconds since 1970, and "now" is the
//! test seam's clock in debug builds that set one (`crate::config::Seam`).

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Dir, State, subdirs};
use crate::clock;
use crate::config::{Seam, StoreConfig};

/// A megabyte, as the configuration counts.
const MB: u64 = 1 << 20;

/// An hour, in seconds.
const HOUR: u64 = 3600;

/// What the configuration says about eviction, in bytes and seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// B: what every directory may take together.
    pub budget: Option<u64>,
    /// F: what each volume keeps free.
    pub floor: Option<u64>,
    /// Tier 1: how long a unit is untouched.
    pub unit_age: u64,
    /// Tier 2: how long an orphan is unclaimed.
    pub orphan_age: u64,
    /// Tiers 3 and 4: how long a directory is idle.
    pub idle_age: u64,
    /// Tier 5: how long a build directory is idle.
    pub dir_age: u64,
    /// Tier 0: how long a scratch copy is untouched.
    pub scratch_age: u64,
    /// Tier 7: how long a build directory of no known workspace is
    /// untouched.
    pub unknown_age: u64,
}

impl Policy {
    pub fn of(config: &StoreConfig) -> Self {
        let hours =
            |hours: Option<u64>, default: u64| hours.unwrap_or(default).saturating_mul(HOUR);
        Self {
            budget: config.budget_mb.map(|mb| mb.saturating_mul(MB)),
            floor: config.floor_mb.map(|mb| mb.saturating_mul(MB)),
            unit_age: hours(config.unit_hours, 24),
            orphan_age: hours(config.orphan_hours, 24),
            idle_age: hours(config.idle_hours, 6),
            dir_age: hours(config.dir_hours, 168),
            scratch_age: hours(config.scratch_hours, 1),
            unknown_age: hours(config.unknown_hours, 1),
        }
    }

    /// The policy under the floor: the same limits, and no ages at all.
    pub fn urgent(self) -> Self {
        Self {
            unit_age: 0,
            orphan_age: 0,
            idle_age: 0,
            dir_age: 0,
            scratch_age: 0,
            unknown_age: 0,
            ..self
        }
    }

    /// Whether anything can make heli evict: a budget or a floor.
    pub fn active(&self) -> bool {
        self.budget.is_some() || self.floor.is_some()
    }

    /// What every directory may take together once eviction is done: 85 %
    /// of the budget.
    pub fn target(budget: u64) -> u64 {
        budget / 100 * 85 + budget % 100 * 85 / 100
    }
}

#[cfg(test)]
mod tests {
    use crate::config::StoreConfig;

    #[test]
    fn llvm_cov_target_is_a_built_in_tool_cache() {
        let config = StoreConfig {
            tool_caches: vec!["tests/trybuild".to_owned()],
            ..StoreConfig::default()
        };
        assert_eq!(
            config.tool_cache_patterns(),
            ["llvm-cov-target", "tests/trybuild"]
        );
    }
}

/// The time now, in seconds since 1970: the seam's clock when it has one.
pub fn now(seam: Option<&Seam>) -> u64 {
    seam.and_then(|seam| seam.clock_ms)
        .map_or_else(clock::now, |ms| ms / 1000)
}

/// A rung of the ladder, in the ladder's order. Its number names it, as
/// `gc`, `status` and the event log show it, and does not order it: the
/// newest rungs, superseded intermediates (6) and build directories of no
/// known workspace (7): 6 comes first, 7 after scratch copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Superseded,
    Scratch,
    Unknown,
    Unit,
    Orphan,
    Incremental,
    ToolCache,
    BuildDir,
}

impl Tier {
    pub fn number(self) -> u8 {
        match self {
            Self::Scratch => 0,
            Self::Unknown => 7,
            Self::Unit => 1,
            Self::Orphan => 2,
            Self::Incremental => 3,
            Self::ToolCache => 4,
            Self::BuildDir => 5,
            Self::Superseded => 6,
        }
    }
}

impl Serialize for Tier {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.number())
    }
}

/// The bytes in the files under a path, and when the newest was written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measure {
    pub bytes: u64,
    /// Seconds since 1970; for a directory without files, its own time.
    pub newest: Option<u64>,
}

impl Measure {
    fn add(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.newest = self.newest.max(other.newest);
    }
}

/// The files under `path` (or `path` itself, a file), never through a link:
/// a link counts nothing.
pub fn measure(path: &Path) -> Measure {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Measure::default();
    };
    if metadata.is_file() {
        return Measure {
            bytes: metadata.len(),
            newest: modified(&metadata),
        };
    }
    if !metadata.is_dir() {
        return Measure::default();
    }
    let mut total = Measure::default();
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && let Ok(metadata) = entry.metadata()
            {
                total.add(Measure {
                    bytes: metadata.len(),
                    newest: modified(&metadata),
                });
            }
        }
    }
    if total.newest.is_none() {
        total.newest = modified(&metadata);
    }
    total
}

fn modified(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata.modified().ok().map(clock::unix_seconds)
}

/// A directory the ladder looks at, as it is now.
#[derive(Debug)]
pub struct Surveyed {
    pub dir: Dir,
    pub state: State,
    /// Its files, but not a `target/` directory's trash.
    pub measure: Measure,
    /// Since when it is idle: its newest file, or its last heli run.
    pub last_use: u64,
    /// For an orphan, since when it is unclaimed (`super::orphan`).
    pub orphaned_since: Option<u64>,
    /// The root of its volume.
    pub volume: Option<PathBuf>,
}

impl Surveyed {
    /// `dir` as it is at `now`; `orphaned_since` says since when an orphan
    /// is unclaimed.
    pub fn of(dir: Dir, now: u64, orphaned_since: impl FnOnce(&Dir) -> Option<u64>) -> Self {
        let measure = measure_root(&dir);
        let state = if super::busy_for_eviction(&dir, now) {
            State::Busy
        } else {
            dir.state()
        };
        let last_run = dir.owner.as_ref().map(|owner| owner.last_run);
        let last_use = measure.newest.max(last_run).unwrap_or(0);
        let orphaned_since = (state == State::Orphan)
            .then(|| orphaned_since(&dir))
            .flatten();
        let volume = super::volume::root(&dir.path);
        Self {
            dir,
            state,
            measure,
            last_use,
            orphaned_since,
            volume,
        }
    }
}

/// The files of the directory `dir`, as [`Surveyed::measure`] counts them.
pub(super) fn measure_root(dir: &Dir) -> Measure {
    let mut total = Measure::default();
    if let Ok(entries) = std::fs::read_dir(&dir.path) {
        for entry in entries.filter_map(Result::ok) {
            if dir.own_trash().is_some_and(|own| entry.file_name() == own) {
                continue;
            }
            let mut measured = measure(&entry.path());
            // heli's own notes in `.heli/` (owner, orphan, attribution) are
            // no use of the directory: their bytes count, their times not.
            if entry.file_name() == ".heli" {
                measured.newest = None;
            }
            total.add(measured);
        }
    }
    if total.newest.is_none() {
        total.newest = std::fs::symlink_metadata(&dir.path)
            .ok()
            .as_ref()
            .and_then(modified);
    }
    total
}

/// Something the ladder would evict.
#[derive(Debug, Clone, Serialize)]
pub struct Victim {
    pub tier: Tier,
    pub path: PathBuf,
    /// The directory it is in, or is: an index into the surveyed ones.
    #[serde(skip)]
    pub dir: usize,
    /// The bytes evicting it frees.
    pub bytes: u64,
    /// Its age as its tier counts it, in seconds.
    pub age: u64,
    /// Its tier's rule, for people.
    pub reason: String,
    /// Chosen for a volume under its floor, its tier's age waived
    /// ([`urgent`]): it serves that floor, never the budget.
    pub floor: bool,
}

/// Every victim the ladder allows in `dirs` at `now`, in the ladder's order;
/// `tool_caches` are the configuration's patterns.
pub fn candidates(
    dirs: &[Surveyed],
    policy: &Policy,
    tool_caches: &[String],
    now: u64,
) -> Vec<Victim> {
    let mut victims = collect(dirs, policy, tool_caches, now, None, |_| true);
    victims.sort_by(|a, b| {
        (a.tier, std::cmp::Reverse(a.age), &a.path).cmp(&(
            b.tier,
            std::cmp::Reverse(b.age),
            &b.path,
        ))
    });
    victims
}

/// Every victim the ladder allows at `now` on the volumes `floors` names
/// (each with what it still needs), whatever its tier's age: what goes
/// under the floor once [`candidates`] are spent, in that order (the
/// module docs). Each serves only its volume's floor ([`Victim::floor`]).
/// Superseded intermediates have no age to waive, and are candidates
/// already.
pub fn urgent(
    dirs: &[Surveyed],
    policy: &Policy,
    tool_caches: &[String],
    now: u64,
    floors: &[(PathBuf, u64)],
) -> Vec<Victim> {
    let under = |surveyed: &Surveyed| {
        floors
            .iter()
            .any(|(root, need)| *need > 0 && surveyed.volume.as_deref() == Some(root.as_path()))
    };
    let mut victims = collect(dirs, &policy.urgent(), tool_caches, now, Some(now), under);
    victims.retain(|victim| victim.tier != Tier::Superseded);
    // What no live workspace will miss first, live directories whole last.
    let rank = |victim: &Victim| match (victim.tier, dirs[victim.dir].state) {
        (Tier::Scratch | Tier::Unknown | Tier::Orphan, _) => 0,
        (Tier::Unit | Tier::Incremental | Tier::ToolCache, _) => 1,
        (Tier::BuildDir | Tier::Superseded, _) => 2,
    };
    victims.sort_by(|a, b| {
        (rank(a), std::cmp::Reverse(a.age), &a.path).cmp(&(
            rank(b),
            std::cmp::Reverse(b.age),
            &b.path,
        ))
    });
    victims
}

/// The victims `policy` allows in the directories of `dirs` that `select`
/// chooses, in no particular order. With `floor` (the time now), they are
/// chosen for a volume under its floor: an orphan no pass has noted yet
/// is unclaimed since now, and each victim says so.
fn collect(
    dirs: &[Surveyed],
    policy: &Policy,
    tool_caches: &[String],
    now: u64,
    floor: Option<u64>,
    select: impl Fn(&Surveyed) -> bool,
) -> Vec<Victim> {
    let mut victims = Vec::new();
    for (index, surveyed) in dirs.iter().enumerate() {
        if surveyed.state == State::Busy {
            continue;
        }
        let root = &surveyed.dir.path;
        let idle = now.saturating_sub(surveyed.last_use);
        if !surveyed.dir.kind.managed() || !select(surveyed) {
            continue;
        }
        let mut victim = |tier: Tier, path: PathBuf, bytes: u64, age: u64, reason: String| {
            let reason = match (floor, &surveyed.volume) {
                (Some(_), Some(volume)) => {
                    format!("{reason}; {} under its floor", volume.display())
                }
                _ => reason,
            };
            victims.push(Victim {
                tier,
                path,
                dir: index,
                bytes,
                age,
                reason,
                floor: floor.is_some(),
            });
        };
        for incremental in incremental_dirs(root) {
            let mut all = Measure::default();
            let Ok(entries) = std::fs::read_dir(&incremental) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let unit = measure(&entry.path());
                all.add(unit);
                if !kind.is_dir() {
                    continue;
                }
                let untouched = now.saturating_sub(unit.newest.unwrap_or(0));
                if untouched >= policy.unit_age {
                    victim(
                        Tier::Unit,
                        entry.path(),
                        unit.bytes,
                        untouched,
                        format!("incremental unit untouched for {}", hours(untouched)),
                    );
                }
            }
            if idle >= policy.idle_age {
                victim(
                    Tier::Incremental,
                    incremental,
                    all.bytes,
                    idle,
                    format!("incremental/ of a directory idle for {}", hours(idle)),
                );
            }
        }
        for (tier, path, bytes, age, reason) in
            super::kind::rungs(surveyed, policy, tool_caches, now, idle, floor)
        {
            victim(tier, path, bytes, age, reason);
        }
    }
    victims
}

/// `seconds` for people, in whole hours.
pub(super) fn hours(seconds: u64) -> String {
    format!("{} h", seconds / HOUR)
}

/// The `incremental/` directories of the profiles in `root`: in
/// `<root>/<profile>/`, and `<root>/<triple>/<profile>/` where a triple
/// holds a `-`. Never through a link.
fn incremental_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for profile in subdirs(root) {
        let incremental = profile.join("incremental");
        if super::is_real_dir(&incremental) {
            dirs.push(incremental);
            continue;
        }
        let triple = profile
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains('-'));
        if triple {
            for profile in subdirs(&profile) {
                let incremental = profile.join("incremental");
                if super::is_real_dir(&incremental) {
                    dirs.push(incremental);
                }
            }
        }
    }
    dirs.sort();
    dirs
}

/// The tool caches in the `target/` directory `root` that `patterns` name,
/// each with the pattern: directories and files, never links, and never
/// reached through one.
pub(super) fn tool_caches_in(root: &Path, patterns: &[String]) -> Vec<(PathBuf, String)> {
    let mut caches: Vec<(PathBuf, String)> = Vec::new();
    for pattern in patterns {
        let components: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
        let mut level = vec![root.to_path_buf()];
        for (depth, component) in components.iter().enumerate() {
            let last = depth + 1 == components.len();
            let mut next = Vec::new();
            for dir in &level {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.filter_map(Result::ok) {
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let fits = kind.is_dir() || (last && kind.is_file());
                    if fits && super::sizes::glob(component, &name) {
                        next.push(entry.path());
                    }
                }
            }
            level = next;
        }
        for cache in level {
            if !caches.iter().any(|(known, _)| *known == cache) {
                caches.push((cache, pattern.clone()));
            }
        }
    }
    caches
}

/// What the ladder must free: bytes for the budget, and bytes on each
/// volume under the floor.
#[derive(Debug, Clone, Default)]
pub struct Goals {
    pub budget: u64,
    /// The budget cannot be met: the busy directories alone hold more than
    /// the target, so evicting a live directory whole would only have it
    /// rebuilt, over budget all the same. Then a live directory's whole
    /// rung ([`Tier::BuildDir`]) does not serve the budget; its other
    /// rungs and the other directories' do, and the floors are served as
    /// ever (#122).
    pub budget_unmet: bool,
    pub floors: Vec<(PathBuf, u64)>,
}

impl Goals {
    pub fn any(&self) -> bool {
        self.budget > 0 || self.floors.iter().any(|(_, need)| *need > 0)
    }
}

/// The victims to evict, from `candidates` in their order, until `goals`
/// are met: each frees what it holds beyond the victims inside it taken
/// before, and is taken only if it serves a goal not yet met (for an
/// unmet budget, a live directory whole does not; one chosen under the
/// floor serves its floor only). What the victims do not free is left in
/// `goals`.
pub fn plan(candidates: Vec<Victim>, dirs: &[Surveyed], goals: &mut Goals) -> Vec<Victim> {
    let mut planned: Vec<Victim> = Vec::new();
    for mut victim in candidates {
        if !goals.any() {
            break;
        }
        if planned
            .iter()
            .any(|taken| victim.path.starts_with(&taken.path))
        {
            continue;
        }
        let inside: u64 = planned
            .iter()
            .filter(|taken| taken.path.starts_with(&victim.path))
            .map(|taken| taken.bytes)
            .sum();
        victim.bytes = victim.bytes.saturating_sub(inside);
        if victim.bytes == 0 {
            continue;
        }
        let volume = dirs[victim.dir].volume.as_deref();
        let floor = goals
            .floors
            .iter_mut()
            .find(|(root, need)| *need > 0 && Some(root.as_path()) == volume);
        let serves_floor = floor.is_some();
        let kept_whole = goals.budget_unmet
            && victim.tier == Tier::BuildDir
            && dirs[victim.dir].state == State::Live;
        let serves_budget = goals.budget > 0 && !kept_whole && !victim.floor;
        if !serves_budget && !serves_floor {
            continue;
        }
        if let Some((_, need)) = floor {
            *need = need.saturating_sub(victim.bytes);
        }
        goals.budget = goals.budget.saturating_sub(victim.bytes);
        planned.push(victim);
    }
    planned
}
