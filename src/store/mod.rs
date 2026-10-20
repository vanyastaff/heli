//! The build store (docs/build-store.md): cargo's intermediates, kept outside
//! the worktrees in one store per volume.
//!
//! **Placement** ([`serving`], [`build_dir`]). `cargo heli setup` sets
//! cargo's `build.build-dir` to `<store>/{workspace-path-hash}`, so each
//! workspace gets a build directory of its own in the store, at
//! `<store>/<2 hex digits>/<14 hex digits>`; `target/` keeps the final
//! artifacts. The store must be on the worktree's volume ([`volume`]): the
//! first one configured there (`[store] paths`), else the volume's default,
//! `<volume>\.heli\build` on Windows. On Unix the volume's root is rarely
//! writable, so the default there is the user's cache,
//! `$XDG_CACHE_HOME/heli/build` (else `~/.cache/heli/build`), when it is on
//! that volume, and `<volume>/.heli/build` otherwise.
//!
//! **Accounting** ([`survey`], [`Dir`], [`owner`], [`registry`], [`sizes`]):
//! every directory the store accounts for (the build directories, the
//! registered `target/` directories, the scratch copies cargo tools leave
//! and the isolated Cargo homes of worktrees, [`home`]), whose it is, its
//! state and its sizes by category.
//!
//! **Eviction** (docs/build-store.md#eviction): the [`ladder`] says what to
//! evict over the budget or under the floor, an [`evict`] pass renames it into
//! the [`trash`] under cargo's locks, and the trash is deleted afterwards. An
//! [`orphan`] counts as unclaimed from the first pass that finds it.

pub mod attribute;
pub mod evict;
pub mod home;
pub mod kind;
pub mod ladder;
pub mod lease;
pub mod managed;
pub mod orphan;
pub mod owner;
pub mod placement;
pub mod probe;
pub mod record;
pub mod recycle;
pub mod registry;
pub mod runner;
pub mod scan;
pub mod scratch;
pub mod sizes;
pub mod superseded;
pub mod trash;
pub mod volume;

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::Serialize;

use crate::config::StoreConfig;
use owner::Owner;
use registry::Registry;
use sizes::Sizes;

/// The lock files cargo holds while it builds, in each profile directory
/// (`<dir>/<profile>/`, or `<dir>/<target triple>/<profile>/`): the build
/// directory's, and the artifact directory's two. Where the build directory
/// is the target directory, all three are in one place.
pub const LOCKS: [&str; 3] = [".cargo-build-lock", ".cargo-lock", ".cargo-artifact-lock"];

pub use kind::Kind;

/// Whether a directory is in use, and whether its workspace still exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// A cargo builds in it: one of its [`LOCKS`] cannot be taken.
    Busy,
    /// Its workspace exists.
    Live,
    /// Its workspace is gone: the worktree was removed.
    Orphan,
    /// A build directory without an owner manifest, which no heli run has
    /// built in: whose it is, heli cannot tell.
    Unknown,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Live => "live",
            Self::Orphan => "orphan",
            Self::Unknown => "unknown",
        }
    }
}

/// A directory the store accounts for.
#[derive(Debug, Clone)]
pub struct Dir {
    pub kind: Kind,
    pub path: PathBuf,
    /// The store a build directory is in.
    pub store: Option<PathBuf>,
    /// The workspace it belongs to: from a build directory's owner manifest,
    /// or from the registry for a `target/` directory.
    pub workspace: Option<PathBuf>,
    /// A build directory's owner manifest.
    pub owner: Option<Owner>,
    /// Where a registered, non-shared `target/` directory's workspace
    /// builds instead, if elsewhere ([`superseded`]).
    pub separation: Option<superseded::Separation>,
}

impl Dir {
    /// The sizes of its files, by category (`sizes`), with the tool caches
    /// of its kind ([`kind`]).
    pub fn scan(&self, config: &StoreConfig) -> Sizes {
        sizes::scan(&self.path, &self.tool_caches(config))
    }

    /// The bytes of its superseded intermediates ([`superseded`]).
    pub fn superseded(&self, config: &StoreConfig) -> u64 {
        self.separation.as_ref().map_or(0, |separation| {
            superseded::bytes(&self.path, separation, &self.tool_caches(config))
        })
    }

    /// Its state, as of now: busy where its kind looks for cargo's locks
    /// ([`Kind::lock_scope`]), else by whether its workspace exists
    /// ([`Kind::workspace_exists`]). (Eviction also counts a directory of
    /// an unknown lock layout busy while it changed within the hour:
    /// [`busy_for_eviction`].)
    pub fn state(&self) -> State {
        let held = match self.kind.lock_scope() {
            kind::LockScope::PackageCache => home::busy(&self.path),
            kind::LockScope::Anywhere | kind::LockScope::Profiles => busy(&self.path),
        };
        if held {
            return State::Busy;
        }
        match &self.workspace {
            Some(workspace) if self.kind.workspace_exists(workspace) => State::Live,
            Some(_) => State::Orphan,
            None => State::Unknown,
        }
    }
}

/// The stores heli manages: the configured ones, then the default store of
/// the volume of each registered workspace, where it exists.
pub fn stores(config: &StoreConfig, registry: &Registry) -> Vec<PathBuf> {
    let mut stores: Vec<PathBuf> = Vec::new();
    let defaults = registry
        .targets
        .iter()
        .filter_map(|target| serving(config, &target.workspace).ok())
        .filter(|store| store.is_dir());
    for store in config.paths.iter().cloned().chain(defaults) {
        if !stores.iter().any(|known| same_path(known, &store)) {
            stores.push(store);
        }
    }
    stores
}

/// Every directory the store accounts for: the build directories in each of
/// the [`stores`], then the registered `target/` directories that exist,
/// the scratch copies cargo tools left ([`scratch`]) and the isolated Cargo
/// homes under `state_dir` ([`home`]).
pub fn survey(config: &StoreConfig, registry: &Registry, state_dir: &Path) -> Vec<Dir> {
    let mut dirs = Vec::new();
    for store in stores(config, registry) {
        for path in build_dirs(&store) {
            let owner =
                owner::read(&path).or_else(|| attribute::attribute(&path, crate::clock::now()));
            dirs.push(Dir {
                kind: Kind::Build,
                workspace: owner.as_ref().map(|owner| owner.workspace.clone()),
                owner,
                store: Some(store.clone()),
                path,
                separation: None,
            });
        }
    }
    for target in &registry.targets {
        if is_real_dir(&target.path) {
            dirs.push(Dir {
                kind: if target.shared {
                    Kind::SharedTarget
                } else {
                    Kind::Target
                },
                path: target.path.clone(),
                store: None,
                workspace: Some(target.workspace.clone()),
                owner: None,
                separation: match (target.shared, &target.build_dir, target.separated_since) {
                    (false, Some(build_dir), Some(since)) => Some(superseded::Separation {
                        build_dir: build_dir.clone(),
                        since,
                    }),
                    _ => None,
                },
            });
        }
    }
    for path in scratch::find() {
        dirs.push(Dir {
            kind: Kind::Scratch,
            path,
            store: None,
            workspace: None,
            owner: None,
            separation: None,
        });
    }
    for path in home::find(state_dir) {
        let owner = owner::read(&path);
        dirs.push(Dir {
            kind: Kind::Home,
            workspace: owner.as_ref().map(|owner| owner.workspace.clone()),
            owner,
            path,
            store: None,
            separation: None,
        });
    }
    dirs
}

/// The build directories in `store`, `<store>/<2 hex digits>/<hex digits>`,
/// in order.
pub fn build_dirs(store: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for outer in subdirs(store) {
        if !is_hex(&outer, Some(2)) {
            continue;
        }
        dirs.extend(subdirs(&outer).into_iter().filter(|dir| is_hex(dir, None)));
    }
    dirs.sort();
    dirs
}

/// Whether `dir`'s name is hex digits, `length` of them when given.
fn is_hex(dir: &Path, length: Option<usize>) -> bool {
    dir.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            !name.is_empty()
                && length.is_none_or(|length| name.len() == length)
                && name.chars().all(|c| c.is_ascii_hexdigit())
        })
}

/// The directories in `dir`, never through a symlink or a junction.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// Whether `path` is a directory itself, not a link to one.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Whether a cargo builds in `dir`: one of the [`LOCKS`] in its profile
/// directories cannot be taken. heli only tries the locks, never waits for
/// them, and lets go at once.
pub fn busy(dir: &Path) -> bool {
    lock_dirs(dir).iter().any(|profile| held(profile))
}

/// Whether eviction counts `dir` busy at `now` (seconds since 1970): a run
/// is in flight in its workspace ([`in_use`]), a Cargo holds a known lock
/// where its kind looks for one ([`Kind::lock_scope`]), or its lock layout
/// is unknown and it changed within the last [`RECENT`] seconds.
///
/// **Unknown layout.** Lock names change between cargo versions. A profile
/// directory that holds cargo's intermediates (`deps/`, `incremental/`,
/// `.fingerprint/`, `build/`) but none of the [`LOCKS`] was laid out by a
/// cargo heli does not know.
pub fn busy_for_eviction(dir: &Dir, now: u64) -> bool {
    if in_use(dir) {
        return true;
    }
    // A home has no profile directories, so no unknown layout either.
    if dir.kind.lock_scope() == kind::LockScope::PackageCache {
        return home::busy(&dir.path);
    }
    let profiles = lock_dirs(&dir.path);
    let busy = match dir.kind.lock_scope() {
        kind::LockScope::Anywhere => locks_inside(&dir.path).map_or(true, |locks| {
            locks
                .iter()
                .any(|path| matches!(try_lock(path), Lock::Held))
        }),
        kind::LockScope::Profiles => profiles.iter().any(|profile| held(profile)),
        kind::LockScope::PackageCache => unreachable!("decided above"),
    };
    busy || (unknown_layout(&profiles) && recently_changed(&dir.path, now))
}

/// Whether a run is in flight in the workspace of `dir` (`lease`): heli
/// leaves it alone, whatever cargo's locks say. An activity namespace that
/// does not resolve leaves everything alone. (A victim elsewhere that
/// changed since the oldest run in flight started is left alone at eviction
/// too: [`changed_in_flight`].)
pub fn in_use(dir: &Dir) -> bool {
    if !lease::available() {
        return true;
    }
    dir.workspace
        .as_deref()
        .is_some_and(|workspace| lease::busy_for(workspace).is_some())
}

/// Whether something whose newest file was written at `newest` changed
/// since the oldest run in flight started: a run's nested cargo may have
/// built it, for a temporary workspace or another known one, and the run
/// may read it. Both are real times (file times and the runs' markers), never
/// the test seam's clock, which only ages the ladder's tiers.
pub fn changed_in_flight(newest: Option<u64>) -> bool {
    lease::in_flight_since().is_some_and(|since| newest.is_some_and(|newest| newest >= since))
}

/// Known Cargo locks at any depth, without following links. An unreadable
/// subtree cannot prove that a whole-directory eviction is safe.
pub fn locks_inside(dir: &Path) -> io::Result<Vec<PathBuf>> {
    if std::fs::symlink_metadata(dir)?.is_file() {
        return Ok(Vec::new());
    }
    let mut locks = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && LOCKS.iter().any(|name| entry.file_name() == *name) {
                locks.push(entry.path());
            }
        }
    }
    Ok(locks)
}

/// Within this many seconds, a change makes a directory of an unknown lock
/// layout busy for eviction: an hour.
pub const RECENT: u64 = 3600;

/// Whether anything in `dir` changed within [`RECENT`] seconds of `now`.
pub fn recently_changed(dir: &Path, now: u64) -> bool {
    ladder::measure(dir)
        .newest
        .is_some_and(|newest| newest.saturating_add(RECENT) > now)
}

/// Whether one of `profiles` holds intermediates but none of the [`LOCKS`].
pub fn unknown_layout(profiles: &[PathBuf]) -> bool {
    const INTERMEDIATES: [&str; 4] = ["deps", "incremental", ".fingerprint", "build"];
    profiles.iter().any(|profile| {
        INTERMEDIATES.iter().any(|name| profile.join(name).is_dir())
            && !LOCKS.iter().any(|name| profile.join(name).is_file())
    })
}

/// The directories of `dir` where cargo's [`LOCKS`] can be: its
/// subdirectories (profile directories), and theirs (a target triple's),
/// never through a link.
pub fn lock_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for profile in subdirs(dir) {
        dirs.extend(subdirs(&profile));
        dirs.push(profile);
    }
    dirs
}

/// Whether one of the [`LOCKS`] in `profile` is held.
fn held(profile: &Path) -> bool {
    LOCKS
        .iter()
        .any(|name| matches!(try_lock(&profile.join(name)), Lock::Held))
}

/// One of cargo's lock files, as heli tried it.
pub enum Lock {
    /// There is none.
    Absent,
    /// Taken by heli until the file closes.
    Taken(File),
    /// Someone holds it, or heli cannot even open it, so it cannot prove it
    /// free.
    Held,
}

/// Tries to take the lock file at `path` exclusively, without waiting.
pub fn try_lock(path: &Path) -> Lock {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Lock::Absent,
        Err(_) => return Lock::Held,
    };
    match file.try_lock() {
        Ok(()) => Lock::Taken(file),
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => Lock::Held,
    }
}

/// The template cargo replaces with a hash of the workspace's manifest path.
pub const WORKSPACE_PATH_HASH: &str = "{workspace-path-hash}";

/// The store that holds `build_dir`, if `build_dir` is a build directory in
/// one heli manages: `<store>/<2 hex digits>/<hex digits>`, where the store
/// is configured, or the default of its volume.
pub fn store_of(config: &StoreConfig, build_dir: &Path) -> Option<PathBuf> {
    let inner = build_dir.parent()?;
    let store = inner.parent()?;
    if !is_hex(inner, Some(2)) || !is_hex(build_dir, None) {
        return None;
    }
    let known = config
        .paths
        .iter()
        .cloned()
        .chain(serving(config, store).ok())
        .any(|known| same_path(&known, store));
    known.then(|| store.to_path_buf())
}

/// Whether `a` and `b` name the same place, as spelled: components compared
/// one by one, on Windows without case.
pub fn same_path(a: &Path, b: &Path) -> bool {
    let spelled = |path: &Path| -> Vec<String> {
        path.components()
            .map(|component| {
                let text = component.as_os_str().to_string_lossy();
                if cfg!(windows) {
                    text.to_lowercase()
                } else {
                    text.into_owned()
                }
            })
            .collect()
    };
    spelled(a) == spelled(b)
}

/// `path` canonical, and on Windows without the `\\?\` prefix where it has
/// a plain spelling (`C:\…`, `\\server\share\…`): the form people know.
pub fn simplified(path: &Path) -> PathBuf {
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return path.to_path_buf();
    };
    let mut components = canonical.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return canonical;
    };
    let plain = match prefix.kind() {
        Prefix::VerbatimDisk(disk) => PathBuf::from(format!("{}:\\", char::from(disk))),
        Prefix::VerbatimUNC(server, share) => match (server.to_str(), share.to_str()) {
            (Some(server), Some(share)) => PathBuf::from(format!(r"\\{server}\{share}\")),
            _ => return canonical,
        },
        _ => return canonical,
    };
    components
        .filter(|component| *component != Component::RootDir)
        .fold(plain, |path, component| path.join(component))
}

/// The store that serves the volume holding `dir`.
pub fn serving(config: &StoreConfig, dir: &Path) -> Result<PathBuf, String> {
    let Some(root) = volume::root(dir) else {
        return Err(format!("cannot tell the volume of {}", dir.display()));
    };
    if let Some(store) = config.paths.iter().find(|store| volume::same(store, &root)) {
        return Ok(store.clone());
    }
    Ok(default_store(&root))
}

/// The `build.build-dir` that places a workspace's build directory in
/// `store`.
pub fn build_dir(store: &Path) -> PathBuf {
    store.join(WORKSPACE_PATH_HASH)
}

#[cfg(windows)]
fn default_store(root: &Path) -> PathBuf {
    root.join(".heli").join("build")
}

#[cfg(unix)]
fn default_store(root: &Path) -> PathBuf {
    let var = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
    };
    let cache = var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")));
    match cache {
        Some(cache) if volume::same(&cache, root) => cache.join("heli").join("build"),
        _ => root.join(".heli").join("build"),
    }
}
