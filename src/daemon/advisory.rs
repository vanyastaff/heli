//! Advisory databases for `cargo audit` and `cargo deny` (docs/runs.md,
//! "Advisory checks"): a verdict of these tools is a verdict on the tree and
//! on the advisory database, so the key holds the database's commit
//! (`super::key`). A run that would fetch the database itself first has the
//! daemon fetch it, before the key is computed, so that the key names the
//! database the run checks against, and a cached pass never hides an
//! advisory published since.
//!
//! The daemon fetches at most once per [`interval`] for each tool and Cargo
//! home, and one fetch at a time: agents that check together share one
//! fetch. A fetch at least [`FLOOR`] old counts as fresh regardless, since a
//! run computes its key more than once. A fetch that fails leaves the run
//! unkeyed, so not cached: the tool then fetches and checks as it would
//! without heli.
//!
//! How each tool fetches without checking anything:
//! - cargo-deny: `cargo deny fetch db`;
//! - cargo-audit, which has no such command: `cargo audit --file <lockfile>`
//!   on a lockfile without packages, which fetches the database (to `--db`,
//!   if the run names one) and finds nothing.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::protocol::RunRequest;

/// How long a fetch counts as fresh at least: one run's key computations.
pub const FLOOR: Duration = Duration::from_secs(30);

/// `[advisories] fetch_interval_minutes`, set as the daemon starts.
static INTERVAL_SECS: AtomicU64 = AtomicU64::new(0);

/// When each (tool, database location) was last fetched successfully.
static FETCHED: Mutex<Option<HashMap<(String, PathBuf), Instant>>> = Mutex::new(None);

/// Sets how long a fetch stays fresh (`[advisories]` in the configuration).
pub fn set_interval(interval: Duration) {
    INTERVAL_SECS.store(interval.as_secs(), Ordering::Relaxed);
}

/// How long a fetch stays fresh: the configured interval, [`FLOOR`] at least.
pub fn interval() -> Duration {
    Duration::from_secs(INTERVAL_SECS.load(Ordering::Relaxed)).max(FLOOR)
}

/// Whether `cargo audit <rest>` or `cargo deny <rest>` fetches the advisory
/// database as it runs.
pub fn fetches(tool: &str, rest: &[OsString]) -> bool {
    let args: Vec<&str> = rest
        .iter()
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| arg.to_str())
        .collect();
    match tool {
        "audit" => !args.iter().any(|arg| matches!(*arg, "--no-fetch" | "-n")),
        "deny" => {
            let offline = args
                .iter()
                .any(|arg| matches!(*arg, "--disable-fetch" | "-d" | "--offline" | "--frozen"));
            let Some(check) = args.iter().position(|arg| *arg == "check") else {
                return false;
            };
            let which = &args[check + 1..]
                .iter()
                .filter(|arg| !arg.starts_with('-'))
                .collect::<Vec<_>>();
            let advisories = which.is_empty()
                || which
                    .iter()
                    .any(|word| matches!(**word, "advisories" | "all"));
            !offline && advisories
        }
        _ => false,
    }
}

/// Fetches the database `cargo <tool>` reads for `request`, unless it was
/// fetched within [`interval`]; `db` is cargo-audit's `--db`, if given, and
/// `home` the run's Cargo home. `scratch` holds the empty lockfile.
pub fn refresh(
    request: &RunRequest,
    tool: &str,
    db: Option<&Path>,
    home: Option<&Path>,
    scratch: &Path,
) -> Result<(), String> {
    let location = db.or(home).map(Path::to_path_buf).unwrap_or_default();
    let entry = (tool.to_owned(), location);
    // Held across the fetch: one fetch at a time, and the others then find
    // it fresh.
    let mut fetched = FETCHED.lock().unwrap_or_else(PoisonError::into_inner);
    let fetched = fetched.get_or_insert_with(HashMap::new);
    if fetched
        .get(&entry)
        .is_some_and(|at| at.elapsed() < interval())
    {
        return Ok(());
    }
    let cargo = request
        .argv
        .first()
        .map_or(OsStr::new("cargo"), |program| program.0.as_os_str());
    let pinned = request
        .argv
        .get(1)
        .map(|arg| arg.0.as_os_str())
        .filter(|arg| arg.as_encoded_bytes().starts_with(b"+"));
    let mut args: Vec<OsString> = pinned.into_iter().map(OsStr::to_owned).collect();
    match tool {
        "deny" => args.extend(["deny", "fetch", "db"].map(OsString::from)),
        "audit" => {
            std::fs::create_dir_all(scratch)
                .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
            let lockfile = scratch.join("advisory-fetch.lock");
            std::fs::write(&lockfile, "version = 3\n")
                .map_err(|error| format!("cannot write {}: {error}", lockfile.display()))?;
            args.extend(["audit", "--file"].map(OsString::from));
            args.push(lockfile.into_os_string());
            if let Some(db) = db {
                args.push("--db".into());
                args.push(db.as_os_str().to_owned());
            }
        }
        _ => return Ok(()),
    }
    let env = request.env.iter().map(|(name, value)| (&name.0, &value.0));
    let output = crate::environment::command(cargo, &args, &request.cwd(), env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            format!("cannot run cargo {tool} to fetch its advisory database: {error}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "cannot fetch the advisory database of cargo {tool} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    fetched.insert(entry, Instant::now());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn which_runs_fetch() {
        assert!(fetches("audit", &args(&[])));
        assert!(!fetches("audit", &args(&["--no-fetch"])));
        assert!(fetches("deny", &args(&["check"])));
        assert!(fetches("deny", &args(&["check", "advisories"])));
        assert!(!fetches("deny", &args(&["check", "bans", "licenses"])));
        assert!(!fetches("deny", &args(&["--offline", "check"])));
        assert!(!fetches("deny", &args(&["check", "--disable-fetch"])));
        assert!(!fetches("deny", &args(&["list"])));
    }

    #[test]
    fn the_interval_has_a_floor() {
        set_interval(Duration::ZERO);
        assert_eq!(interval(), FLOOR);
        set_interval(Duration::from_secs(600));
        assert_eq!(interval(), Duration::from_secs(600));
        set_interval(Duration::ZERO);
    }
}
