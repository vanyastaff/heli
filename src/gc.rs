//! `cargo heli gc --dry-run [--json]`: every directory the build store
//! accounts for (`crate::store::survey`), with its state and size. Nothing
//! is deleted: eviction (DESIGN.md §6.3) does not exist yet, so `gc`
//! without `--dry-run` is refused.
//!
//! It reads the store itself, as of now, and needs no daemon: the
//! configuration names the stores, the registry in the state directory the
//! registered `target/` directories, and each directory's locks, owner
//! manifest and files say the rest.
//!
//! **`--json`**, a versioned document (`schema`, now 1) whose fields are only
//! added:
//!
//! ```json
//! {"schema":1,"dirs":[
//!   {"kind":"build","path":"D:\\.heli\\build\\28\\4ec2fa37745150",
//!    "store":"D:\\.heli\\build","state":"live","workspace":"D:\\flui",
//!    "repository":"D:\\flui\\.git","created_at":1791910980,
//!    "last_run":1791911112}]}
//! ```
//!
//! `kind` is `build` or `target`; `state` is `busy`, `live`, `orphan` or
//! `unknown` (`crate::store::State`). `store` is `null` for a `target/`
//! directory; `workspace`, `repository`, `created_at` and `last_run` are
//! `null` where no owner manifest says them.

use serde::Serialize;

use crate::clock;
use crate::config::Config;
use crate::locations::Locations;
use crate::store::registry::Registry;
use crate::store::{self, Dir, Kind, State};

/// The version of the `--json` document.
const SCHEMA: u32 = 1;

/// Exit code of `gc` without `--dry-run`: a refusal.
const REFUSED: i32 = 2;

#[derive(Serialize)]
struct Listing {
    schema: u32,
    dirs: Vec<Entry>,
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
}

impl Entry {
    fn of(dir: &Dir) -> Self {
        let text = |path: &std::path::Path| path.to_string_lossy().into_owned();
        Self {
            kind: dir.kind,
            path: text(&dir.path),
            store: dir.store.as_deref().map(text),
            state: dir.state(),
            workspace: dir.workspace.as_deref().map(text),
            repository: dir
                .owner
                .as_ref()
                .and_then(|owner| owner.repository.as_deref())
                .map(text),
            created_at: dir.owner.as_ref().map(|owner| owner.created_at),
            last_run: dir.owner.as_ref().map(|owner| owner.last_run),
        }
    }
}

/// `cargo heli gc`.
pub fn main(dry_run: bool, json: bool) -> ! {
    if !dry_run {
        eprintln!(
            "cargo-heli: gc deletes nothing yet; `cargo heli gc --dry-run` lists the \
             directories it will look at"
        );
        std::process::exit(REFUSED)
    }
    let (config, registry) = match Locations::resolve().and_then(|locations| {
        let config = Config::load(&locations.config)?;
        Ok((config, Registry::load(&locations.state_dir)))
    }) {
        Ok(found) => found,
        Err(reason) => {
            eprintln!("cargo-heli: {reason}");
            std::process::exit(1)
        }
    };
    let listing = Listing {
        schema: SCHEMA,
        dirs: store::survey(&config.store, &registry)
            .iter()
            .map(Entry::of)
            .collect(),
    };
    if json {
        let text = serde_json::to_string(&listing).expect("BUG: the listing serializes");
        println!("{text}");
    } else {
        print!("{}", to_text(&listing));
    }
    std::process::exit(0)
}

/// One line per directory: kind, state, path, and whose it is.
fn to_text(listing: &Listing) -> String {
    let now = clock::now();
    let mut text = String::new();
    for entry in &listing.dirs {
        let kind = match entry.kind {
            Kind::Build => "build",
            Kind::Target => "target",
        };
        text.push_str(&format!(
            "{kind:<6}  {:<7}  {}",
            entry.state.name(),
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
    text.push_str(&format!(
        "{} directories; nothing deleted (--dry-run)\n",
        listing.dirs.len()
    ));
    text
}
