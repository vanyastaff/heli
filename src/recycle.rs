//! `cargo heli recycle` (internal): what git's `post-checkout` hook runs
//! when `git worktree add` checks a new worktree out (`crate::setup`
//! installs the hook). The worktree holding the working directory gets the
//! most recently used orphan of its repository as its build directory
//! (`crate::store::recycle::for_worktree`), before anything builds in it.
//!
//! It reads the store itself and needs no daemon. A recycling is said on
//! stderr, which git shows, and logged as a `recycle` event with trigger
//! `hook` in the event log (`crate::daemon::events`). It never fails the
//! git command that ran it: whatever goes wrong is said on stderr, and it
//! exits 0.

use crate::clock;
use crate::config::Config;
use crate::daemon::events::Events;
use crate::locations::Locations;
use crate::store::probe::Probe;
use crate::store::recycle;
use crate::store::registry::Registry;
use crate::worktree;

/// `cargo heli recycle`.
pub fn main() -> ! {
    if let Err(reason) = run() {
        eprintln!("cargo-heli: no build directory recycled: {reason}");
    }
    std::process::exit(0)
}

fn run() -> Result<(), String> {
    let locations = Locations::resolve()?;
    let config = Config::load(&locations.config)?;
    let cwd = std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?;
    let worktree = worktree::of(&cwd);
    let registry = Registry::load(&locations.state_dir);
    let probe = Probe::here(worktree.clone());
    let recycled =
        recycle::for_worktree(&config.store, &registry, &probe, &worktree, clock::now())?;
    if let Some(recycled) = recycled {
        eprintln!("cargo-heli: {}", recycled.describe());
        match locations.create_state_dir() {
            Ok(()) => {
                Events::configured(&locations, &config).record("recycle", recycled.event("hook"))
            }
            Err(error) => eprintln!(
                "cargo-heli: cannot log the recycling in {}: {error}",
                locations.state_dir.display()
            ),
        }
    }
    Ok(())
}
