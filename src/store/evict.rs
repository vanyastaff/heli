//! An eviction pass (DESIGN.md §6.3): the store as it is now, what the
//! ladder (`super::ladder`) would evict, and, unless the pass is a dry run,
//! the evictions themselves. `cargo heli gc` makes one pass; the daemon
//! makes one whenever the store is over its budget or a volume under its
//! floor (`crate::daemon::accounts`).
//!
//! **An eviction** never waits. Under cargo's locks, which heli tries and
//! does not wait for, it renames the victim into the trash (`super::trash`)
//! and lets go:
//!
//! - every one of cargo's lock files ([`super::LOCKS`]) in the directory's
//!   profile directories and anywhere inside the victim must be free;
//!   one that is not makes the directory busy, and the victim is skipped;
//! - heli holds them while it renames, but on Windows not those inside the
//!   victim: Windows refuses to rename a directory with a file open inside,
//!   so it lets go of those first, and the rename itself then fails if a
//!   cargo opened one meanwhile;
//! - the victim, and every directory between it and its root, must be a
//!   directory of its own, not a link;
//! - a directory of an unknown lock layout must not have changed within the
//!   hour, an orphan must still be one, with the same owner, and superseded
//!   intermediates must still be untouched since their `target/` directory
//!   was seen apart from its build directory.
//!
//! What is in the trash is deleted afterwards, by whoever made the pass
//! (`super::trash::empty`).
//!
//! Only one pass at a time evicts: `evict.lock` in the state directory.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::ladder::{self, Goals, Policy, Surveyed, Tier, Victim};
use super::registry::Registry;
use super::{Dir, Kind, Lock, State, orphan, trash};
use crate::config::StoreConfig;

/// The file whose lock a pass that evicts holds, in the state directory.
pub fn lock_file(state_dir: &Path) -> PathBuf {
    state_dir.join("evict.lock")
}

/// A volume that holds a store or a registered `target/` directory.
#[derive(Debug, Clone, Serialize)]
pub struct Volume {
    pub root: PathBuf,
    /// Its free bytes, if they can be read.
    pub free: Option<u64>,
    /// The bytes still in its trash, which count as free.
    pub pending: u64,
}

/// What a pass reclaims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// What the ladder chooses over the budget or under the floor.
    Ladder,
    /// Every superseded directory, whatever the budget and the floor
    /// (`gc --superseded`).
    Superseded,
}

/// What a pass found and did.
#[derive(Debug)]
pub struct Pass {
    /// Some directories were left alone: a run is in flight in their
    /// workspace (`super::lease`).
    pub runtime_busy: bool,
    pub policy: Policy,
    pub dirs: Vec<Surveyed>,
    /// What every directory takes together.
    pub used: u64,
    pub volumes: Vec<Volume>,
    /// What the pass had to free.
    pub goals: Goals,
    /// The victims: planned in a dry run, else evicted, in order.
    pub victims: Vec<Victim>,
    /// Victims that could not be evicted, and why.
    pub skipped: Vec<(Victim, String)>,
}

/// Looks at the store at `now`, and plans what `scope` would evict
/// ([`Pass::victims`]); [`execute`] evicts it. A `dry_run` changes nothing
/// at all; otherwise the orphans' notes are brought up to date first
/// (`super::orphan`).
pub fn plan(
    config: &StoreConfig,
    registry: &Registry,
    now: u64,
    dry_run: bool,
    scope: Scope,
) -> Pass {
    let policy = Policy::of(config);
    let mut dirs: Vec<Surveyed> = Vec::new();
    for dir in super::survey(config, registry) {
        // A target directory that is a store's build directory is that.
        if dirs
            .iter()
            .any(|known| super::same_path(&known.dir.path, &dir.path))
        {
            continue;
        }
        dirs.push(Surveyed::of(dir, now, |dir| {
            if dry_run {
                orphan::since(dir)
            } else {
                // A note that cannot be written leaves the orphan unclaimed
                // since now: it waits.
                orphan::note(dir, State::Orphan, now).ok().flatten()
            }
        }));
    }
    if !dry_run {
        for surveyed in &dirs {
            if surveyed.dir.kind == Kind::Build && surveyed.state == State::Live {
                let _ = orphan::note(&surveyed.dir, State::Live, now);
            }
        }
    }
    let runtime_busy = dirs.iter().any(|surveyed| super::in_use(&surveyed.dir));
    // A shared `target/` directory is not heli's to manage (`super::kind`).
    let used = dirs
        .iter()
        .filter(|surveyed| surveyed.dir.kind.managed())
        .map(|surveyed| surveyed.measure.bytes)
        .sum();
    let volumes = volumes(config, registry, &dirs);
    let goals = goals(&policy, used, &volumes);
    let mut pass = Pass {
        runtime_busy,
        policy,
        dirs,
        used,
        volumes,
        goals: goals.clone(),
        victims: Vec::new(),
        skipped: Vec::new(),
    };
    if scope == Scope::Ladder && !goals.any() {
        return pass;
    }
    let mut candidates =
        ladder::candidates(&pass.dirs, &policy, &config.tool_cache_patterns(), now);
    // What changed since the oldest run in flight started may be in use
    // (`super::changed_in_flight`): left out before the ladder chooses, so
    // that it chooses something else.
    if super::lease::in_flight_since().is_some() {
        let (left, kept): (Vec<Victim>, Vec<Victim>) = candidates
            .into_iter()
            .partition(|victim| super::changed_in_flight(newest(&pass.dirs[victim.dir], victim)));
        candidates = kept;
        pass.runtime_busy |= !left.is_empty();
        pass.skipped.extend(
            left.into_iter()
                .map(|victim| (victim, IN_FLIGHT.to_owned())),
        );
    }
    pass.victims = match scope {
        Scope::Ladder => ladder::plan(candidates, &pass.dirs, goals),
        Scope::Superseded => candidates
            .into_iter()
            .filter(|victim| victim.tier == Tier::Superseded)
            .collect(),
    };
    pass
}

/// Evicts the victims `pass` planned, at `now`, in order: those that could
/// not be evicted move to [`Pass::skipped`]. The caller holds the lock of
/// [`lock_file`], and empties the trash afterwards.
pub fn execute(pass: &mut Pass, now: u64) {
    let planned = std::mem::take(&mut pass.victims);
    for victim in planned {
        match evict(&pass.dirs[victim.dir], &victim, now) {
            Ok(()) => pass.victims.push(victim),
            Err(why) => {
                pass.runtime_busy |= why.contains("run in flight");
                pass.skipped.push((victim, why));
            }
        }
    }
}

/// The fields of the event log's `evict` event for `victim`: its `tier`,
/// `path`, `bytes` and `reason`.
pub fn event(victim: &Victim) -> serde_json::Value {
    serde_json::json!({
        "tier": victim.tier,
        "path": victim.path.to_string_lossy(),
        "bytes": victim.bytes,
        "reason": victim.reason,
    })
}

/// The volumes of `dirs`, with their free space and what their trash holds.
fn volumes(config: &StoreConfig, registry: &Registry, dirs: &[Surveyed]) -> Vec<Volume> {
    let mut volumes: Vec<Volume> = Vec::new();
    for root in dirs.iter().filter_map(|surveyed| surveyed.volume.as_ref()) {
        if volumes.iter().any(|known| known.root == *root) {
            continue;
        }
        volumes.push(Volume {
            root: root.clone(),
            free: super::volume::free(root),
            pending: 0,
        });
    }
    for trash in trash::all(config, registry) {
        let pending = trash::pending(&trash);
        if pending == 0 {
            continue;
        }
        let root = super::volume::root(&trash);
        if let Some(volume) = volumes
            .iter_mut()
            .find(|volume| Some(&volume.root) == root.as_ref())
        {
            volume.pending += pending;
        }
    }
    volumes
}

/// What `policy` asks to free when the directories take `used` and the
/// volumes are as `volumes` say.
fn goals(policy: &Policy, used: u64, volumes: &[Volume]) -> Goals {
    let budget = match policy.budget {
        Some(budget) if used > budget => used - Policy::target(budget),
        _ => 0,
    };
    let floors = match policy.floor {
        None => Vec::new(),
        Some(floor) => volumes
            .iter()
            .filter_map(|volume| {
                let free = volume.free?.saturating_add(volume.pending);
                (free < floor).then(|| (volume.root.clone(), floor - free))
            })
            .collect(),
    };
    Goals { budget, floors }
}

/// Evicts `victim` from `surveyed`: renames it into the trash under cargo's
/// locks (see the module docs). The error says why it was skipped.
fn evict(surveyed: &Surveyed, victim: &Victim, now: u64) -> Result<(), String> {
    let root = &surveyed.dir.path;
    if !reached_without_links(root, &victim.path) {
        return Err("gone, or reached through a link".to_owned());
    }
    let Some(trash) = surveyed.dir.trash() else {
        return Err("no trash".to_owned());
    };
    let profiles = super::lock_dirs(root);
    let mut outer = Vec::new();
    let mut inner = Vec::new();
    for profile in &profiles {
        for name in super::LOCKS {
            let lock = profile.join(name);
            if lock.starts_with(&victim.path) {
                inner.push(lock);
            } else {
                outer.push(lock);
            }
        }
    }
    // A whole directory may contain a build-script's nested Cargo cache.
    // Recheck these locks after planning, including on Unix where rename
    // does not fail just because an active descendant has an open file.
    inner.extend(
        super::locks_inside(&victim.path)
            .map_err(|error| format!("cannot inspect nested Cargo locks: {error}"))?,
    );
    inner.sort();
    inner.dedup();
    let _outer = hold(&outer)?;
    let inner = hold(&inner)?;
    if victim.tier == Tier::BuildDir
        && surveyed.state == State::Unknown
        && !super::managed::unknown_is_reclaimable(&surveyed.dir)
    {
        return Err(
            "unknown directory has no verified managed cache root or lock layout".to_owned(),
        );
    }
    if super::unknown_layout(&profiles) && super::recently_changed(root, now) {
        return Err("busy: changed within the hour".to_owned());
    }
    if victim.tier == Tier::Orphan && !still_orphan(&surveyed.dir) {
        return Err("no longer an orphan".to_owned());
    }
    if victim.tier == Tier::Superseded
        && !surveyed
            .dir
            .separation
            .as_ref()
            .is_some_and(|separation| super::superseded::still(&victim.path, separation.since))
    {
        return Err("written since the build directory moved".to_owned());
    }
    // Inspecting a large victim above may be slow. Only the final rename
    // needs the runtime leases of its workspace: a new managed command there
    // takes its shared lease before Cargo starts, so its arrival makes this
    // try-lock skip. Keep scans and Cargo-lock discovery outside this
    // critical section.
    if !super::lease::available() {
        return Err("busy: the runtime activity namespace does not resolve".to_owned());
    }
    let _runtime = match surveyed.dir.workspace.as_deref() {
        Some(workspace) => super::lease::exclusive_for(workspace)?,
        None => Vec::new(),
    };
    if super::changed_in_flight(newest(surveyed, victim)) {
        return Err(IN_FLIGHT.to_owned());
    }
    // Windows does not rename a directory with a file open inside.
    let _inner = (!cfg!(windows)).then_some(inner);
    match trash::put(&victim.path, &trash) {
        Ok(_) => Ok(()),
        // A full volume can refuse even a rename into the trash (NTFS needs
        // room for it): delete the victim where it is, under the same locks
        // (#108).
        Err(error) if disk_full(&error) => remove(&victim.path)
            .map_err(|error| format!("cannot delete it on a full volume: {error}")),
        Err(error) => Err(format!("cannot move it into the trash: {error}")),
    }
}

/// When `victim`, in `surveyed`, last changed: the whole directory as the
/// survey measured it (without heli's own notes, `ladder::Surveyed`), else
/// the files under the victim.
fn newest(surveyed: &Surveyed, victim: &Victim) -> Option<u64> {
    if victim.path == surveyed.dir.path {
        surveyed.measure.newest
    } else {
        ladder::measure(&victim.path).newest
    }
}

/// Why a victim that changed since a run in flight started is left alone.
const IN_FLIGHT: &str = "busy: changed since a run in flight started";

/// Whether `error` says the volume is full.
fn disk_full(error: &std::io::Error) -> bool {
    // ERROR_DISK_FULL (112) and ERROR_HANDLE_DISK_FULL (39) on Windows,
    // ENOSPC elsewhere.
    error.kind() == std::io::ErrorKind::StorageFull
        || (cfg!(windows) && matches!(error.raw_os_error(), Some(39 | 112)))
}

/// Deletes the file or directory `path`.
fn remove(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Takes every lock in `locks` that exists, without waiting; the error says
/// which one is held.
fn hold(locks: &[PathBuf]) -> Result<Vec<File>, String> {
    let mut held = Vec::new();
    for lock in locks {
        match super::try_lock(lock) {
            Lock::Absent => {}
            Lock::Taken(file) => held.push(file),
            Lock::Held => return Err(format!("busy: {} is held", lock.display())),
        }
    }
    Ok(held)
}

/// Whether `victim`, at or below the directory `root`, exists, is no link,
/// and is reached from `root` through directories of their own.
fn reached_without_links(root: &Path, victim: &Path) -> bool {
    let Ok(below) = victim.strip_prefix(root) else {
        return false;
    };
    if !super::is_real_dir(root) {
        return false;
    }
    let mut path = root.to_path_buf();
    let mut components = below.components().peekable();
    while let Some(component) = components.next() {
        path.push(component);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        let last = components.peek().is_none();
        if !(metadata.is_dir() || (last && metadata.is_file())) {
            return false;
        }
    }
    true
}

/// Whether build directory `dir` is still the orphan it was: its owner
/// manifest names the same workspace, which is still gone.
fn still_orphan(dir: &Dir) -> bool {
    let Some(owner) = super::owner::read(&dir.path) else {
        return false;
    };
    dir.owner.as_ref() == Some(&owner) && !owner.workspace.join("Cargo.toml").is_file()
}
