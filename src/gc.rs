//! `cargo heli gc [--dry-run] [--json]`: every directory the build store
//! accounts for (`crate::store::survey`), with its state and size, and an
//! eviction pass over them (DESIGN.md §6.3, `crate::store::evict`): over
//! the budget or under the floor, the victims the ladder chooses, each with
//! its tier and reason. `gc` evicts them, logs each eviction in the event
//! log (`crate::daemon::events`) and deletes the trash before it exits, at
//! low I/O priority; `--dry-run` only names them, and changes nothing.
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
//! {"schema":1,"dry_run":true,"dirs":[
//!   {"kind":"build","path":"D:\\.heli\\build\\28\\4ec2fa37745150",
//!    "store":"D:\\.heli\\build","state":"live","workspace":"D:\\flui",
//!    "repository":"D:\\flui\\.git","created_at":1791910980,
//!    "last_run":1791911112,"size":1000,"by_category":{…}}],
//!  "used":1000,"budget":900,"floor":null,
//!  "volumes":[{"root":"D:\\","free":218103808000,"pending":0}],
//!  "victims":[{"tier":1,"path":"D:\\.heli\\build\\28\\4ec2fa37745150\\debug\\incremental\\flui_view-0x1yz",
//!              "bytes":300,"age":93600,"reason":"incremental unit untouched for 26 h"}],
//!  "skipped":[]}
//! ```
//!
//! `kind` is `build` or `target`; `state` is `busy`, `live`, `orphan` or
//! `unknown` (`crate::store::State`). `store` is `null` for a `target/`
//! directory; `workspace`, `repository`, `created_at` and `last_run` are
//! `null` where no owner manifest says them. `used` is what every directory
//! takes, and `budget` and `floor` are B and F in bytes (`null`: none);
//! each volume has its free bytes (`null`: unknown) and the bytes its trash
//! still holds. `victims` are the ones planned in a dry run, else the ones
//! evicted, in order; `skipped` are those that could not be, each with
//! `why`.

use std::fs::File;

use serde::Serialize;

use crate::config::{Config, StoreConfig};
use crate::daemon::events::{self, Events};
use crate::locations::Locations;
use crate::store::evict::{self, Pass, Volume};
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
    dirs: Vec<Entry>,
    used: u64,
    budget: Option<u64>,
    floor: Option<u64>,
    volumes: &'a [Volume],
    victims: &'a [Victim],
    skipped: Vec<Skipped<'a>>,
}

#[derive(Serialize)]
struct Entry {
    kind: Kind,
    path: String,
    store: Option<String>,
    state: State,
    workspace: Option<String>,
    repository: Option<String>,
    created_at: Option<u64>,
    last_run: Option<u64>,
    /// Bytes in its files.
    size: u64,
    by_category: Sizes,
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
            workspace: dir.workspace.as_deref().map(text),
            repository: dir
                .owner
                .as_ref()
                .and_then(|owner| owner.repository.as_deref())
                .map(text),
            created_at: dir.owner.as_ref().map(|owner| owner.created_at),
            last_run: dir.owner.as_ref().map(|owner| owner.last_run),
            size: sizes.total(),
            by_category: sizes,
        }
    }
}

/// `cargo heli gc`.
pub fn main(dry_run: bool, json: bool) -> ! {
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
    let mut pass = evict::plan(&config.store, &registry, now, dry_run);
    let dirs: Vec<Entry> = pass
        .dirs
        .iter()
        .map(|surveyed| Entry::of(&surveyed.dir, surveyed.state, &config.store))
        .collect();
    if !dry_run {
        evict::execute(&mut pass, now);
        let events = Events::new(
            locations.event_log(),
            config
                .event_log
                .max_kb
                .map_or(events::DEFAULT_MAX, |kb| kb.get().saturating_mul(1024)),
        );
        for victim in &pass.victims {
            events.record("evict", evict::event(victim));
        }
    }
    let listing = Listing {
        schema: SCHEMA,
        dry_run,
        dirs,
        used: pass.used,
        budget: pass.policy.budget,
        floor: pass.policy.floor,
        volumes: &pass.volumes,
        victims: &pass.victims,
        skipped: pass
            .skipped
            .iter()
            .map(|(victim, why)| Skipped { victim, why })
            .collect(),
    };
    if json {
        let text = serde_json::to_string(&listing).expect("BUG: the listing serializes");
        println!("{text}");
    } else {
        print!("{}", to_text(&listing, &pass, now));
    }
    if !dry_run {
        trash::lower_io_priority();
        for trash in trash::all(&config.store, &registry) {
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

/// One line per directory: kind, state, size, path, and whose it is; the
/// budget and the floor; one line per victim; then the total.
fn to_text(listing: &Listing, pass: &Pass, now: u64) -> String {
    let mut text = String::new();
    for entry in &listing.dirs {
        let kind = match entry.kind {
            Kind::Build => "build",
            Kind::Target => "target",
        };
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
            }
            text.push(')');
        }
        text.push('\n');
    }
    text.push_str(&limits(pass));
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
    text
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
