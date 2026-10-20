//! `cargo heli gc [--superseded] [--dry-run] [--json]`: every directory the build store
//! accounts for (`crate::store::survey`), with its state and size, and an
//! eviction pass over them (docs/build-store.md#eviction,
//! `crate::store::evict`): over the budget or under the floor, the victims the
//! ladder chooses, each with its tier and reason. `gc` evicts them, logs each
//! eviction in the event log (`crate::daemon::events`) and deletes the trash
//! before it exits, at low I/O priority; `--dry-run` only names them, and
//! changes nothing.
//!
//! **`--superseded`** reclaims every superseded directory in the registered
//! `target/` directories (`crate::store::superseded`, tier 6) now, whatever
//! the budget and the floor, and nothing else; with `--dry-run` it names
//! them. Without it, the text form ends with a hint while any are left:
//! `58.3 GiB superseded by the build store: cargo heli gc --superseded`.
//!
//! It reads the store itself, as of now, and needs no daemon: the
//! configuration names the stores, the budget, the floor and the ages, the
//! registry in the state directory the registered `target/` directories,
//! and each directory's locks, owner manifest and files say the rest. A pass
//! that evicts holds `evict.lock` in the state directory, so the daemon's
//! own passes wait their turn.
//!
//! **`--json`**, a versioned document (`schema`, now 1) whose fields are only
//! added:
//!
//! ```json
//! {"schema":1,"dry_run":true,"superseded":false,"dirs":[
//!   {"kind":"build","path":"D:\\.heli\\build\\28\\4ec2fa37745150",
//!    "store":"D:\\.heli\\build","state":"live","workspace":"D:\\flui",
//!    "repository":"D:\\flui\\.git","created_at":1791910980,
//!    "last_run":1791911112,"size":1000,"by_category":{…},"superseded":0}],
//!  "used":1000,"busy":0,"budget":900,"budget_unmet":false,"floor":null,
//!  "volumes":[{"root":"D:\\","free":218103808000,"pending":0}],
//!  "victims":[{"tier":1,"path":"D:\\.heli\\build\\28\\4ec2fa37745150\\debug\\incremental\\flui_view-0x1yz",
//!              "bytes":300,"age":93600,"reason":"incremental unit untouched for 26 h","floor":false}],
//!  "skipped":[],"left":{"budget":0,"floors":[]}}
//! ```
//!
//! `kind` is `build`, `target`, `shared`, `scratch` or `home`
//! (`crate::store::Kind`); `state` is `busy`, `live`, `orphan` or
//! `unknown` (`crate::store::State`). `store` is `null` but for a build
//! directory; `workspace`, `repository`, `created_at` and `last_run` are
//! `null` where no owner manifest says them (for a `home`, the workspace
//! is its worktree, `crate::store::home`), and `last_run` also for a
//! directory attributed to its workspace without a run of heli's
//! (`crate::store::attribute`). A directory's `superseded` is
//! the bytes of its superseded intermediates, which `by_category` counts
//! too (0 but in a `target/` directory apart from its build directory);
//! the document's `superseded` says the pass was `--superseded`. A
//! directory's `kept` (absent above) says why it is not deleted whole,
//! when a rule keeps it: busy, within its age, of no cargo layout, in its
//! worktree, or kept whole over a budget that cannot be met. `used` is
//! what every directory
//! takes, `busy` what the busy ones among them take, and `budget` and
//! `floor` are B and F in bytes (`null`: none); `budget_unmet` says the
//! busy directories alone hold more than the budget's target, so that the
//! pass kept the live directories whole (docs/build-store.md#eviction,
//! #122);
//! each volume has its free bytes (`null`: unknown) and the bytes its trash
//! still holds. `victims` are the ones planned in a dry run, else the ones
//! evicted, in order, a victim chosen under the floor with its age waived
//! marked `floor`; `skipped` are those that could not be, each with
//! `why`. `left` is what the victims do not free: bytes over the budget's
//! target, and bytes under the floor on each volume. The text form then
//! says so, and what is kept, in groups: `left     budget still 469.2 GiB
//! over its target` and `kept     126 of no known workspace within
//! unknown_hours (387.0 GiB); 23 live within dir_hours (300.0 GiB)`.

use std::fs::File;

use serde::Serialize;

use crate::config::{Config, StoreConfig};
use crate::daemon::events::Events;
use crate::locations::Locations;
use crate::store::evict::{self, Pass, Scope, Volume};
use crate::store::ladder::{self, Victim};
use crate::store::registry::Registry;
use crate::store::sizes::{self, Sizes};
use crate::store::{Dir, Kind, State, trash};

/// The version of the `--json` document.
const SCHEMA: u32 = 1;

#[derive(Serialize)]
struct Listing<'a> {
    schema: u32,
    dry_run: bool,
    /// The pass was `--superseded`.
    superseded: bool,
    runtime_busy: bool,
    dirs: Vec<Entry>,
    used: u64,
    /// What the busy directories take.
    busy: u64,
    budget: Option<u64>,
    /// The busy directories alone hold more than the budget's target.
    budget_unmet: bool,
    floor: Option<u64>,
    volumes: &'a [Volume],
    victims: &'a [Victim],
    skipped: Vec<Skipped<'a>>,
    left: Left,
}

/// What the victims do not free.
#[derive(Serialize)]
struct Left {
    budget: u64,
    floors: Vec<Floor>,
}

#[derive(Serialize)]
struct Floor {
    root: String,
    need: u64,
}

#[derive(Serialize)]
struct Entry {
    kind: Kind,
    path: String,
    store: Option<String>,
    state: State,
    /// The store was created by heli as an exclusively regenerable cache.
    managed_cache_root: bool,
    workspace: Option<String>,
    repository: Option<String>,
    created_at: Option<u64>,
    /// When the last run started; none for a directory attributed to its
    /// workspace without a run of heli's.
    last_run: Option<u64>,
    /// The owner manifest records no run (`crate::store::attribute`): the
    /// text says so, where it would say the last run's age.
    #[serde(skip)]
    no_run: bool,
    /// Bytes in its files.
    size: u64,
    by_category: Sizes,
    /// Bytes in its superseded intermediates, also in `by_category`.
    superseded: u64,
    /// Why it is not deleted whole, when a rule keeps it
    /// (`crate::store::evict::kept`).
    #[serde(skip_serializing_if = "Option::is_none")]
    kept: Option<String>,
    /// The group `kept` is counted in.
    #[serde(skip)]
    kept_group: Option<&'static str>,
}

#[derive(Serialize)]
struct Skipped<'a> {
    #[serde(flatten)]
    victim: &'a Victim,
    why: &'a str,
}

impl Entry {
    fn of(dir: &Dir, state: State, config: &StoreConfig) -> Self {
        let sizes = dir.scan(config);
        let text = |path: &std::path::Path| path.to_string_lossy().into_owned();
        Self {
            kind: dir.kind,
            path: text(&dir.path),
            store: dir.store.as_deref().map(text),
            state,
            managed_cache_root: dir
                .store
                .as_deref()
                .is_some_and(crate::store::managed::owns),
            workspace: dir.workspace.as_deref().map(text),
            repository: dir
                .owner
                .as_ref()
                .and_then(|owner| owner.repository.as_deref())
                .map(text),
            created_at: dir.owner.as_ref().map(|owner| owner.created_at),
            // An attributed manifest has 0 for its last run: no run, not
            // one at the epoch (#119).
            last_run: dir
                .owner
                .as_ref()
                .map(|owner| owner.last_run)
                .filter(|&at| at != 0),
            no_run: dir.owner.as_ref().is_some_and(|owner| owner.last_run == 0),
            size: sizes.total(),
            by_category: sizes,
            superseded: dir.superseded(config),
            kept: None,
            kept_group: None,
        }
    }
}

/// `cargo heli gc`; `superseded` for `--superseded`.
pub fn main(dry_run: bool, json: bool, superseded: bool) -> ! {
    let (locations, config, registry) = match Locations::resolve().and_then(|locations| {
        let config = Config::load(&locations.config)?;
        let registry = Registry::load(&locations.state_dir);
        Ok((locations, config, registry))
    }) {
        Ok(found) => found,
        Err(reason) => fail(&reason),
    };
    let now = ladder::now(Some(&config.seam()));
    // Held until gc exits: one pass evicts at a time.
    let _evicting = (!dry_run).then(|| evict_lock(&locations));
    let scope = if superseded {
        Scope::Superseded
    } else {
        Scope::Ladder
    };
    let mut pass = evict::plan(
        &config.store,
        &registry,
        &locations.state_dir,
        now,
        dry_run,
        scope,
    );
    let mut dirs: Vec<Entry> = pass
        .dirs
        .iter()
        .map(|surveyed| Entry::of(&surveyed.dir, surveyed.state, &config.store))
        .collect();
    if !dry_run {
        evict::execute(&mut pass, now);
        let events = Events::configured(&locations, &config);
        for victim in &pass.victims {
            events.record("evict", evict::event(victim));
        }
    }
    for (index, entry) in dirs.iter_mut().enumerate() {
        if let Some(kept) = evict::kept(&pass, index, now) {
            entry.kept_group = Some(kept.group);
            entry.kept = Some(kept.why);
        }
    }
    let listing = Listing {
        schema: SCHEMA,
        dry_run,
        superseded,
        runtime_busy: pass.runtime_busy,
        dirs,
        used: pass.used,
        busy: pass.busy_bytes,
        budget: pass.policy.budget,
        budget_unmet: pass.goals.budget_unmet,
        floor: pass.policy.floor,
        volumes: &pass.volumes,
        victims: &pass.victims,
        skipped: pass
            .skipped
            .iter()
            .map(|(victim, why)| Skipped { victim, why })
            .collect(),
        left: Left {
            budget: pass.left.budget,
            floors: pass
                .left
                .floors
                .iter()
                .map(|(root, need)| Floor {
                    root: root.to_string_lossy().into_owned(),
                    need: *need,
                })
                .collect(),
        },
    };
    if json {
        let text = serde_json::to_string(&listing).expect("BUG: the listing serializes");
        println!("{text}");
    } else {
        print!("{}", to_text(&listing, &pass, now));
    }
    if !dry_run {
        trash::lower_io_priority();
        for trash in trash::all(&config.store, &registry, &locations.state_dir) {
            if let Err(error) = trash::empty(&trash) {
                eprintln!("cargo-heli: cannot empty {} yet: {error}", trash.display());
            }
        }
    }
    std::process::exit(0)
}

/// The lock of a pass that evicts, taken: waits for another pass to end.
fn evict_lock(locations: &Locations) -> File {
    let locked = locations.create_state_dir().and_then(|()| {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(evict::lock_file(&locations.state_dir))?;
        file.lock()?;
        Ok(file)
    });
    locked.unwrap_or_else(|error| fail(&format!("cannot take the eviction lock: {error}")))
}

fn fail(reason: &str) -> ! {
    eprintln!("cargo-heli: {reason}");
    std::process::exit(1)
}

/// One line per directory: kind, state, size, path, and whose it is (and,
/// when the pass leaves a goal unmet, why it is kept); the budget and the
/// floor; one line per victim; what is left and what is kept; then the
/// total.
fn to_text(listing: &Listing, pass: &Pass, now: u64) -> String {
    let mut text = String::new();
    let left = pass.left_text();
    for entry in &listing.dirs {
        let kind = entry.kind.name();
        text.push_str(&format!(
            "{kind:<6}  {:<7}  {:>10}  {}",
            entry.state.name(),
            sizes::human(entry.size),
            entry.path
        ));
        if let Some(workspace) = &entry.workspace {
            text.push_str(&format!("  ({workspace}"));
            if let Some(last_run) = entry.last_run {
                text.push_str(&format!(", last run {}s ago", now.saturating_sub(last_run)));
            } else if entry.no_run {
                text.push_str(", last run: none");
            }
            text.push(')');
        }
        if left.is_some()
            && let Some(kept) = &entry.kept
        {
            text.push_str(&format!("  kept: {kept}"));
        }
        text.push('\n');
    }
    if listing.superseded {
        text.push_str("reclaiming superseded intermediates, whatever the budget (--superseded)\n");
    } else {
        text.push_str(&limits(pass));
    }
    let verb = if listing.dry_run { "evict" } else { "evicted" };
    for victim in listing.victims {
        text.push_str(&format!(
            "{verb:<7}  tier {}  {:>10}  {}  ({})\n",
            victim.tier.number(),
            sizes::human(victim.bytes),
            victim.path.display(),
            victim.reason
        ));
    }
    for skipped in &listing.skipped {
        text.push_str(&format!(
            "skipped  tier {}  {}  ({}; {})\n",
            skipped.victim.tier.number(),
            skipped.victim.path.display(),
            skipped.victim.reason,
            skipped.why
        ));
    }
    if let Some(left) = &left {
        text.push_str(&format!("left     {left}\n"));
        let groups = kept_groups(&listing.dirs);
        if !groups.is_empty() {
            text.push_str(&format!("kept     {}\n", groups.join("; ")));
        }
    }
    let total = listing.dirs.iter().map(|entry| entry.size).sum();
    let freed: u64 = listing.victims.iter().map(|victim| victim.bytes).sum();
    let mut summary = format!(
        "{} directories, {}; ",
        listing.dirs.len(),
        sizes::human(total)
    );
    if listing.dry_run {
        if !listing.victims.is_empty() {
            summary.push_str(&format!(
                "would evict {}, {}; ",
                listing.victims.len(),
                sizes::human(freed)
            ));
        }
        summary.push_str("nothing deleted (--dry-run)\n");
    } else {
        summary.push_str(&format!(
            "evicted {}, {}",
            listing.victims.len(),
            sizes::human(freed)
        ));
        if !listing.skipped.is_empty() {
            summary.push_str(&format!(", skipped {}", listing.skipped.len()));
        }
        summary.push('\n');
    }
    text.push_str(&summary);
    let superseded: u64 = listing.dirs.iter().map(|entry| entry.superseded).sum();
    if !listing.superseded && superseded > 0 {
        text.push_str(&hint(superseded));
    }
    text
}

/// What is kept, by group, most bytes first: `126 of no known workspace
/// within unknown_hours (387.0 GiB)`.
fn kept_groups(dirs: &[Entry]) -> Vec<String> {
    let mut groups: Vec<(&'static str, usize, u64)> = Vec::new();
    for entry in dirs {
        let Some(group) = entry.kept_group else {
            continue;
        };
        match groups.iter_mut().find(|(known, _, _)| *known == group) {
            Some((_, count, bytes)) => {
                *count += 1;
                *bytes += entry.size;
            }
            None => groups.push((group, 1, entry.size)),
        }
    }
    groups.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(b.0)));
    groups
        .into_iter()
        .map(|(group, count, bytes)| format!("{count} {group} ({})", sizes::human(bytes)))
        .collect()
}

/// The line that points at `gc --superseded` while `bytes` are superseded.
pub fn hint(bytes: u64) -> String {
    format!(
        "{} superseded by the build store: cargo heli gc --superseded\n",
        sizes::human(bytes)
    )
}

/// The budget and the floor, and how the store stands against them.
fn limits(pass: &Pass) -> String {
    let policy = &pass.policy;
    if !policy.active() {
        return "no budget or floor ([store] budget_mb, floor_mb): nothing to evict\n".to_owned();
    }
    let mut text = String::new();
    match policy.budget {
        Some(budget) => text.push_str(&format!(
            "budget  {} of {} used",
            sizes::human(pass.used),
            sizes::human(budget)
        )),
        None => text.push_str(&format!("budget  none, {} used", sizes::human(pass.used))),
    }
    if let Some(unmet) = pass.unmet_budget() {
        text.push_str(&format!(
            "; cannot be met: {}; live directories are kept whole",
            unmet.holders()
        ));
    }
    if let Some(floor) = policy.floor {
        for volume in &pass.volumes {
            let free = volume
                .free
                .map_or_else(|| "unknown".to_owned(), sizes::human);
            text.push_str(&format!(
                "; {} {free} free (floor {})",
                volume.root.display(),
                sizes::human(floor)
            ));
        }
    }
    if !pass.goals.any() {
        text.push_str("; nothing to evict");
    }
    text.push('\n');
    text
}
