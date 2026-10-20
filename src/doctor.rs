//! `cargo heli doctor [--json]`: how the installed cargo tools and the
//! cargo configuration interact with heli (spec #38).
//!
//! It reads, and changes nothing: the `cargo-*` programs on the search path
//! and in the Cargo home's `bin`, the Cargo configuration cargo would read
//! here (the Cargo home's, and every `.cargo/` from the working directory
//! up), and the variables that override it. Each finding says what happens
//! and what to do about it.
//!
//! **Usage memory.** The daemon's event log records each run's profile
//! (`admit`, and `direct` for the runs a client makes itself in the pool).
//! doctor counts the runs of tools no profile names (`unknown`), so that a
//! profile gets written for the ones used most: in the user's
//! configuration, the repository's `.config/heli.toml`, or heli itself.
//!
//! **`--json`**, a versioned document (`schema`, now 1) whose fields are only
//! added:
//!
//! ```json
//! {"schema":1,
//!  "tools":[{"name":"insta","profile":"accept/reject mutate …"},
//!           {"name":"bloat","profile":null}],
//!  "findings":[{"id":"jobs","level":"note","message":"…","action":"…"}],
//!  "unknown_tools":[{"name":"bloat","runs":12}]}
//! ```
//!
//! The helper programs of a multi-binary tool (`cargo-ndk-env` beside
//! `cargo-ndk`) are not listed when the tool itself is installed.
//!
//! `profile` is `null` for a tool heli has no built-in profile for. `level`
//! is `note` (how heli behaves) or `warn` (something the user likely
//! expects to work and will not).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::class;
use crate::config::CargoHome;
use crate::profiles::{Profiles, Rule};

/// The version of the `--json` document.
const SCHEMA: u32 = 1;

/// Variables that override the Cargo configuration doctor explains.
const VARS: [&str; 10] = [
    "CARGO_MAKEFLAGS",
    "MAKEFLAGS",
    "MFLAGS",
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
    "RUSTC_WRAPPER",
    "CARGO_HOME",
    "RUST_TEST_THREADS",
];

/// What heli does with `cargo nextest …`. nextest is cargo's own
/// classification (`class::test_runner`), not a built-in tool profile.
const NEXTEST_PROFILE: &str = "nextest run: test run with test slots; other commands exec";

/// Tools that install helper programs named `cargo-<tool>-<helper>`
/// (cargo-ndk's `cargo-ndk-env`, `cargo-ndk-runner`, `cargo-ndk-test`). The
/// helpers are not commands a user runs, so doctor leaves them out of the
/// list when the tool itself is installed.
const MULTI_BINARY: [&str; 1] = ["ndk"];

/// What doctor looks at; built from the real environment by [`gather`], and
/// from fakes by the tests.
#[derive(Debug, Default)]
pub struct Inputs {
    /// The directories of the search path, and the Cargo home's `bin`.
    pub bin_dirs: Vec<PathBuf>,
    /// The Cargo configuration cargo reads, nearest the working directory
    /// first, each file with its content.
    pub configs: Vec<(PathBuf, toml::Table)>,
    /// The variables of [`VARS`] that are set and not empty.
    pub env: BTreeMap<String, String>,
    /// heli's locations that Windows package virtualization redirects
    /// (`crate::locations::package_redirection`): what each is, where it is
    /// spelled, and where it actually is.
    pub redirections: Vec<(&'static str, PathBuf, PathBuf)>,
    /// The Cargo home, where cargo-audit keeps its advisory database.
    pub cargo_home: Option<PathBuf>,
    /// The `CARGO_MAKEFLAGS` that points a cargo at heli's host pool, as
    /// `cargo heli setup --print` shows it.
    pub makeflags: Option<String>,
    /// `[cargo] home` of heli's configuration.
    pub home_mode: CargoHome,
    /// The build store that serves the working directory, when it exists:
    /// a configured `[store] paths` entry or the volume's default store.
    pub store: Option<PathBuf>,
    /// The store's volume's free bytes, and `[store] floor_mb` in bytes,
    /// when both are known.
    pub store_space: Option<(u64, u64)>,
    /// The event log's lines, oldest first (the rotated file, then the
    /// current one).
    pub events: Vec<String>,
    /// `[profiles]` of heli's configuration.
    pub profiles: Profiles,
    /// The profiles of the repository's `.config/heli.toml`.
    pub repository_profiles: Vec<Rule>,
    /// The running daemon's cargo-heli version, when one answers.
    pub daemon_version: Option<String>,
    /// The budget the running daemon's eviction passes cannot meet, when
    /// its status says so (`crate::status::UnmetBudget`, #122).
    pub unmet_budget: Option<crate::status::UnmetBudget>,
    /// The versions of cargo-heli that the Cargo home's `.crates.toml`
    /// lists with no program: installs that could not replace the running
    /// one ([`failed_installs`]).
    pub failed_installs: Vec<String>,
    /// The programs an upgrade put aside beside this one and could not
    /// delete yet (`crate::upgrade::aside`).
    pub old_programs: Vec<PathBuf>,
    /// The running daemon's host pool's slots and saturation, when one
    /// answers.
    pub pool: Option<(u32, crate::status::Saturation)>,
    /// Now, in seconds since 1970: what "the last hour" of the event log
    /// counts back from.
    pub now: u64,
}

/// How far back doctor reads the event log for the pool's saturation.
const HOUR: u64 = 3600;

/// What the event log and the pool say of the pool's saturation over the
/// last hour (docs/host-pool.md#saturation).
#[derive(Debug, Default, PartialEq)]
struct Saturation {
    /// Seconds of the last hour that the pool was saturated.
    seconds: u64,
    /// The most runs active, and waiting, at once, and the pool's slots.
    most_active: u64,
    most_waiting: u64,
    slots: u64,
    /// The tests nextest's slow-timeout killed in runs during saturation.
    timeouts: Vec<String>,
}

/// The pool's saturation over the hour before `now`, from the `saturated`
/// and `timeouts` events in `events` and the spell going on in `pool`.
fn saturation(
    events: &[String],
    pool: Option<&(u32, crate::status::Saturation)>,
    now: u64,
) -> Saturation {
    let start = now.saturating_sub(HOUR);
    let mut found = Saturation::default();
    for line in events {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let number = |key: &str| event[key].as_u64().unwrap_or(0);
        match event["event"].as_str() {
            Some("saturated") => {
                let since = number("since");
                let end = since.saturating_add(number("seconds"));
                if end <= start {
                    continue;
                }
                found.seconds += end.min(now).saturating_sub(since.max(start));
                found.most_active = found.most_active.max(number("most_active"));
                found.most_waiting = found.most_waiting.max(number("most_waiting"));
                found.slots = found.slots.max(number("slots"));
            }
            Some("timeouts") if number("at") >= start => {
                if let Some(tests) = event["tests"].as_array() {
                    found.timeouts.extend(
                        tests
                            .iter()
                            .filter_map(|test| Some(test.as_str()?.to_owned())),
                    );
                }
            }
            _ => {}
        }
    }
    if let Some((slots, now)) = pool
        && let Some(lasted) = now.saturated_s
    {
        found.seconds += lasted.min(HOUR);
        found.most_active = found.most_active.max(u64::from(now.active_runs));
        found.most_waiting = found.most_waiting.max(u64::from(now.waiting_runs));
        found.slots = found.slots.max(u64::from(*slots));
    }
    found.seconds = found.seconds.min(HOUR);
    found
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub profile: Option<&'static str>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Finding {
    pub id: &'static str,
    pub level: &'static str,
    pub message: String,
    pub action: String,
}

/// How often a tool no profile names ran.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub name: String,
    pub runs: u64,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: u32,
    pub tools: Vec<Tool>,
    pub findings: Vec<Finding>,
    /// The tools no profile names, most used first, ten at most.
    pub unknown_tools: Vec<Usage>,
}

/// How many unknown tools doctor lists.
const UNKNOWN_SHOWN: usize = 10;

/// The tools of the runs in `events` that were recorded without a profile
/// and that heli has none for now (no built-in profile, not a cargo
/// subcommand, not named by the `user` or `repository` profiles), by runs,
/// most first (then by name), [`UNKNOWN_SHOWN`] at most.
fn unknown_usage(events: &[String], user: &Profiles, repository: &[Rule]) -> Vec<Usage> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for line in events {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if !matches!(event["event"].as_str(), Some("admit" | "direct"))
            || event["profile"].as_str() != Some("unknown")
        {
            continue;
        }
        let words: Vec<&str> = event["argv"]
            .as_array()
            .map(|argv| argv.iter().skip(1).filter_map(|arg| arg.as_str()).collect())
            .unwrap_or_default();
        let tool = words.iter().find(|word| !word.starts_with('+'));
        if let Some(tool) = tool
            && class::tool_profile(tool).is_none()
            && !class::is_cargo_builtin(tool)
            && !user
                .tool
                .iter()
                .chain(repository)
                .any(|rule| rule.tool == *tool)
        {
            *counts.entry((*tool).to_owned()).or_default() += 1;
        }
    }
    let mut usage: Vec<Usage> = counts
        .into_iter()
        .map(|(name, runs)| Usage { name, runs })
        .collect();
    usage.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.name.cmp(&b.name)));
    usage.truncate(UNKNOWN_SHOWN);
    usage
}

/// Reads the real environment, from `cwd`.
pub fn gather(cwd: &Path) -> Inputs {
    let mut inputs = Inputs::default();
    if let Some(path) = std::env::var_os("PATH") {
        inputs.bin_dirs.extend(std::env::split_paths(&path));
    }
    let home = crate::setup::cargo_home(cwd);
    inputs.cargo_home.clone_from(&home);
    if let Some(home) = &home {
        inputs.bin_dirs.push(home.join("bin"));
    }
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(|dir| dir.join(".cargo")).collect();
    dirs.extend(home);
    for dir in dirs {
        let file = crate::setup::cargo_config_file(&dir);
        if let Ok(text) = std::fs::read_to_string(&file)
            && let Ok(table) = text.parse::<toml::Table>()
        {
            inputs.configs.push((file, table));
        }
    }
    if let Ok(path) = crate::locations::Locations::config_path() {
        if let Ok(client) = crate::config::Config::load_client(&path) {
            inputs.home_mode = client.cargo.home;
            inputs.profiles = client.profiles;
        }
        let store = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|table| table.get("store").cloned())
            .and_then(|value| value.try_into::<crate::config::StoreConfig>().ok())
            .unwrap_or_default();
        inputs.store = crate::store::serving(&store, cwd)
            .ok()
            .filter(|store| store.is_dir());
        inputs.store_space = inputs.store.as_deref().and_then(|dir| {
            let floor = store.floor_mb?.saturating_mul(1 << 20);
            Some((crate::store::volume::free(dir)?, floor))
        });
    }
    inputs.repository_profiles =
        crate::profiles::repository(&crate::worktree::of(cwd)).unwrap_or_default();
    if let Ok(locations) = crate::locations::Locations::resolve() {
        inputs.makeflags = crate::pool::makeflags(&locations.pool).ok();
        for (what, path) in [
            ("state directory", &locations.state_dir),
            ("configuration", &locations.config),
        ] {
            if let Some(actual) = crate::locations::package_redirection(path) {
                inputs.redirections.push((what, path.clone(), actual));
            }
        }
        let log = locations.event_log();
        let mut rotated = log.clone().into_os_string();
        rotated.push(".1");
        for file in [PathBuf::from(rotated), log] {
            if let Ok(text) = std::fs::read_to_string(file) {
                inputs.events.extend(text.lines().map(str::to_owned));
            }
        }
    }
    let status = crate::client::daemon_status();
    inputs.daemon_version = status
        .as_ref()
        .and_then(|status| status.daemon.as_ref()?.version.clone());
    inputs.pool = status
        .as_ref()
        .and_then(|status| Some((status.pool.as_ref()?.slots, status.saturation.clone()?)));
    inputs.unmet_budget = status.and_then(|status| status.unmet_budget);
    inputs.now = crate::clock::now();
    if let Some(home) = inputs.cargo_home.clone()
        && let Ok(text) = std::fs::read_to_string(home.join(".crates.toml"))
    {
        inputs.failed_installs = failed_installs(&text);
    }
    if let Ok(program) = std::env::current_exe() {
        inputs.old_programs = crate::upgrade::aside(&program);
    }
    for var in VARS {
        if let Some(value) = std::env::var_os(var).filter(|value| !value.is_empty()) {
            inputs
                .env
                .insert(var.to_owned(), value.to_string_lossy().into_owned());
        }
    }
    inputs
}

/// The versions of cargo-heli that `crates_toml`, the text of the Cargo
/// home's `.crates.toml`, lists with no program: `cargo install` writes
/// the entry once it has built the crate, and on Windows then fails to
/// replace a running program.
fn failed_installs(crates_toml: &str) -> Vec<String> {
    let Ok(table) = crates_toml.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(toml::Value::Table(installed)) = table.get("v1") else {
        return Vec::new();
    };
    installed
        .iter()
        .filter(|(_, programs)| programs.as_array().is_some_and(Vec::is_empty))
        .filter_map(|(name, _)| name.strip_prefix("cargo-heli ")?.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// The names `X` of the `cargo-X` programs in `dirs`, sorted, without heli
/// itself and the tools that ship with the toolchain.
fn installed_tools(dirs: &[PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            // A program: `.exe` on Windows (not the `.pdb` beside it), no
            // extension elsewhere.
            let stem = if cfg!(windows) {
                match file.strip_suffix(".exe") {
                    Some(stem) => stem,
                    None => continue,
                }
            } else if file.contains('.') {
                continue;
            } else {
                &file
            };
            if let Some(name) = stem.strip_prefix("cargo-")
                && !name.is_empty()
                && !matches!(name, "heli" | "clippy" | "fmt" | "miri")
                && entry.path().is_file()
            {
                names.insert(name.to_owned());
            }
        }
    }
    names
}

/// The value of `RUST_TEST_THREADS` in a Cargo `[env]` table of `configs`:
/// a string, or a table with `value`.
fn env_table_test_threads(configs: &[(PathBuf, toml::Table)]) -> Option<String> {
    configs.iter().find_map(|(_, table)| {
        match table.get("env")?.as_table()?.get("RUST_TEST_THREADS")? {
            toml::Value::String(text) => Some(text.clone()),
            toml::Value::Table(entry) => entry.get("value")?.as_str().map(str::to_owned),
            _ => None,
        }
    })
}

/// `build.<key>` from the nearest file that sets it as a string or number.
fn build_setting(configs: &[(PathBuf, toml::Table)], key: &str) -> Option<String> {
    configs.iter().find_map(
        |(_, table)| match table.get("build")?.as_table()?.get(key)? {
            toml::Value::String(text) => Some(text.clone()),
            toml::Value::Integer(number) => Some(number.to_string()),
            _ => None,
        },
    )
}

/// Whether a `linker` or `rustflags` anywhere in `value` names `linker`.
fn names_linker(value: &toml::Value, linker: &str, in_setting: bool) -> bool {
    match value {
        toml::Value::String(text) => in_setting && text.contains(linker),
        toml::Value::Array(items) => items
            .iter()
            .any(|item| names_linker(item, linker, in_setting)),
        toml::Value::Table(table) => table.iter().any(|(key, value)| {
            names_linker(
                value,
                linker,
                in_setting || matches!(key.as_str(), "linker" | "rustflags"),
            )
        }),
        _ => false,
    }
}

/// The jobserver that make flags `flags` name: what `--jobserver-auth=`
/// (or the older `--jobserver-fds=`) gives.
fn jobserver_auth(flags: &str) -> Option<&str> {
    flags.split_whitespace().find_map(|word| {
        word.strip_prefix("--jobserver-auth=")
            .or_else(|| word.strip_prefix("--jobserver-fds="))
    })
}

/// Explains `inputs`.
pub fn diagnose(inputs: &Inputs) -> Report {
    let installed = installed_tools(&inputs.bin_dirs);
    let tools: Vec<Tool> = installed
        .iter()
        .filter(|name| {
            !MULTI_BINARY.iter().any(|tool| {
                installed.contains(*tool)
                    && name
                        .strip_prefix(tool)
                        .is_some_and(|helper| helper.starts_with('-'))
            })
        })
        .cloned()
        .map(|name| Tool {
            profile: if name == "nextest" {
                Some(NEXTEST_PROFILE)
            } else {
                class::tool_profile(&name)
            },
            name,
        })
        .collect();
    let mut findings = Vec::new();
    let mut add = |id, level, message: String, action: &str| {
        findings.push(Finding {
            id,
            level,
            message,
            action: action.to_owned(),
        });
    };
    let env = |name: &str| inputs.env.get(name).cloned();
    let setting = |key: &str| build_setting(&inputs.configs, key);

    if let Some(jobs) = env("CARGO_BUILD_JOBS").or_else(|| setting("jobs")) {
        add(
            "jobs",
            "note",
            format!(
                "build.jobs = {jobs} has no effect on a run under the pool: the pool decides how many jobs run."
            ),
            "Nothing to do; size the pool in heli's configuration instead.",
        );
    }
    let target_dir = env("CARGO_TARGET_DIR")
        .or_else(|| env("CARGO_BUILD_TARGET_DIR"))
        .or_else(|| setting("target-dir"));
    if let Some(dir) = target_dir {
        add(
            "target-dir",
            "warn",
            format!(
                "A global target directory is set ({dir}). Workspaces sharing it share one build directory, so cargo's artifact lock serializes them. Once heli has seen it for two workspaces, the build store leaves it alone: it evicts nothing there and leaves it out of the budget."
            ),
            "Unset it, or set it per worktree.",
        );
    }
    if let Some(dir) = env("CARGO_BUILD_BUILD_DIR").or_else(|| setting("build-dir")) {
        if dir.to_lowercase().contains("heli") {
            add(
                "build-dir",
                "note",
                format!("build.build-dir points into heli's build store ({dir})."),
                "Nothing to do.",
            );
        } else {
            add(
                "build-dir",
                "note",
                format!(
                    "build.build-dir is your own ({dir}); `cargo heli setup` will not overwrite it, and heli's build store does not manage it."
                ),
                "Nothing to do, or remove it and run `cargo heli setup` to use the store.",
            );
        }
    }
    if let Some(wrapper) = env("RUSTC_WRAPPER").or_else(|| setting("rustc-wrapper")) {
        if wrapper.to_lowercase().contains("sccache") {
            add(
                "rustc-wrapper",
                "warn",
                format!(
                    "build.rustc-wrapper is {wrapper}. An sccache server started inside a run ends with that run's processes, so `cargo heli cancel` can kill it."
                ),
                "Start the server yourself (`sccache --start-server`) before runs.",
            );
        } else {
            add(
                "rustc-wrapper",
                "note",
                format!("build.rustc-wrapper is {wrapper}; it is part of the verification key."),
                "Nothing to do.",
            );
        }
    }
    for linker in ["mold", "lld"] {
        let table = |(_, table): &(PathBuf, toml::Table)| {
            names_linker(&toml::Value::Table(table.clone()), linker, false)
        };
        if inputs.configs.iter().any(table) {
            add(
                "linker",
                "warn",
                format!(
                    "{linker} does not follow the jobserver: a link uses every core, whatever the pool allows."
                ),
                "Expect memory spikes while linking; the memory governor is the only lever.",
            );
        }
    }
    if let Some(home) = env("CARGO_HOME") {
        match inputs.home_mode {
            CargoHome::Isolated => add(
                "cargo-home",
                "note",
                format!(
                    "heli's configuration has [cargo] home = \"isolated\": managed runs use a private Cargo home per worktree. CARGO_HOME ({home}) is only the original home they start from."
                ),
                "Nothing to do; set `home = \"shared\"` under [cargo] to use CARGO_HOME as is.",
            ),
            CargoHome::Shared => add(
                "cargo-home",
                "note",
                format!(
                    "CARGO_HOME is {home}, and heli's [cargo] home is shared (the default): managed runs use it as is."
                ),
                "Nothing to do; `--cargo-home isolated`, or `home = \"isolated\"` under [cargo], gives managed runs a private home per worktree instead.",
            ),
        }
    }
    if let Some(store) = &inputs.store
        && env("CARGO_BUILD_BUILD_DIR")
            .or_else(|| setting("build-dir"))
            .is_none()
    {
        let line = crate::setup::build_dir_line(store);
        let store = store.display();
        add(
            "store-unused",
            "warn",
            format!(
                "The build store {store} exists, but no Cargo configuration reaching this workspace sets build.build-dir, so its intermediates stay in target/ and the store is unused."
            ),
            &format!(
                "Run `cargo heli setup`, or add `{line}` under [build] in a Cargo configuration (or set CARGO_BUILD_BUILD_DIR)."
            ),
        );
    }
    if let (Some(store), Some((free, floor))) = (&inputs.store, inputs.store_space)
        && free < floor
    {
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        add(
            "store-floor",
            "warn",
            format!(
                "The volume of the build store {} has {:.1} GiB free, under its floor of {:.1} GiB ([store] floor_mb): builds may fail for lack of space, and eviction is behind.",
                store.display(),
                gib(free),
                gib(floor)
            ),
            "Run `cargo heli gc` (or `cargo heli gc --superseded`), and check `cargo heli gc --dry-run` for what is left alone and why.",
        );
    }
    if let Some(unmet) = &inputs.unmet_budget {
        add(
            "store-budget",
            "warn",
            format!(
                "The build store's {}: eviction cannot bring the store under the budget while they build, so it keeps the live workspaces' idle directories whole (evicted after dir_hours, each would be rebuilt as soon as its workspace is used again) and takes only their old units, incremental/ and tool caches, with the orphans, scratch copies and superseded intermediates. The store takes {:.1} GiB.",
                unmet.describe(),
                unmet.used as f64 / f64::from(1u32 << 30)
            ),
            "Raise [store] budget_mb above what the workspaces that build at once take, and set [store] floor_mb so that the volume stays protected while the store is over its budget.",
        );
    }
    let saturation = saturation(&inputs.events, inputs.pool.as_ref(), inputs.now);
    if saturation.seconds > HOUR / 2 {
        add(
            "pool-saturated",
            "note",
            format!(
                "The host pool was saturated for {} min of the last hour: up to {} runs shared its {} slots, {} of them waiting for a slot at once.",
                saturation.seconds / 60,
                saturation.most_active,
                saturation.slots,
                saturation.most_waiting
            ),
            "Narrow the runs: `--affected` (it also takes nextest's -E), short -p lists instead of --workspace, and --priority background for sweeps. `cargo heli status` shows the waiting runs and the slot waits.",
        );
    }
    if !saturation.timeouts.is_empty() {
        let mut tests = saturation.timeouts.clone();
        tests.sort();
        tests.dedup();
        let shown = tests.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        add(
            "nested-cargo-timeouts",
            "warn",
            format!(
                "nextest's slow-timeout killed {} test(s) while the pool was saturated in the last hour: {shown}{}. A test that runs cargo itself waits for pool slots like any build and times out under load.",
                tests.len(),
                if tests.len() > 5 { ", …" } else { "" }
            ),
            "Give such tests a longer slow-timeout in .config/nextest.toml (an override for them), or run them alone; and narrow the runs that saturate the pool.",
        );
    }
    if let Some(threads) =
        env("RUST_TEST_THREADS").or_else(|| env_table_test_threads(&inputs.configs))
    {
        add(
            "test-threads",
            "note",
            format!(
                "RUST_TEST_THREADS = {threads}: in a test run, heli's test slots decide the threads. `cargo heli test` sets it to K (and replaces a `--test-threads` after `--`), and K is at most the variable's value when the client's environment has it (a Cargo [env] entry is not seen by heli, so it only gets overridden). `cargo heli nextest run` passes `--test-threads K` and ignores the variable. Other commands keep it."
            ),
            "Nothing to do; to bound the threads of a run, pass --test-threads (or -j for nextest) or size the pool.",
        );
    }
    // Cargo takes the first of these that is set (#86).
    if let Some((var, flags)) = ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"]
        .into_iter()
        .find_map(|var| Some((var, env(var)?)))
        && let Some(auth) = jobserver_auth(&flags)
        && let Some(ours) = &inputs.makeflags
        && let Some(pool) = jobserver_auth(ours)
        // The host pool, or a run's own pool inside it (`<pool>-<run>`).
        && auth != pool
        && !auth
            .strip_prefix(pool)
            .is_some_and(|run| run.starts_with('-'))
    {
        add(
            "makeflags",
            "warn",
            format!(
                "{var} names the jobserver {auth}, not heli's host pool {pool}. A cargo started with it cannot connect (it prints one warning) and runs as many jobs as it likes, beside the pool. The pool's name follows the state directory: moving it, or setting HELI_STATE_DIR or HELI_POOL, changes the name."
            ),
            &format!(
                "Run `cargo heli setup` to rewrite what it wrote before, and set {var} to `{ours}` wherever else you set it (an editor's rust-analyzer.cargo.extraEnv); then start the programs that read it again."
            ),
        );
    }
    let analyzer = |dir: &PathBuf| {
        dir.join("rust-analyzer").is_file() || dir.join("rust-analyzer.exe").is_file()
    };
    if inputs.bin_dirs.iter().any(analyzer) {
        add(
            "rust-analyzer",
            "note",
            "rust-analyzer started by your IDE builds outside the pool: it does not inherit the environment `cargo heli setup` writes."
                .to_owned(),
            "Set rust-analyzer.cargo.extraEnv in the editor to the CARGO_MAKEFLAGS that `cargo heli setup --print` shows.",
        );
    }
    if let Some(db) = inputs
        .cargo_home
        .as_ref()
        .map(|home| home.join("advisory-db"))
        .filter(|db| db.is_dir() && !db.join(".git").exists() && !db.join("HEAD").is_file())
    {
        add(
            "advisory-db",
            "warn",
            format!(
                "{} is not a git clone of the advisory database: cargo audit cannot fetch into it, and heli cannot key (so never caches) its checks.",
                db.display()
            ),
            "Move it aside and let `cargo audit` clone the database again.",
        );
    }
    for (what, spelled, actual) in &inputs.redirections {
        add(
            "package-redirection",
            "warn",
            format!(
                "heli's {what} {} is redirected by Windows package virtualization to {}: only processes of this app see it. A terminal, an editor or a scheduled task outside it reaches the same daemon but reads another configuration and state.",
                spelled.display(),
                actual.display()
            ),
            "Set HELI_STATE_DIR and HELI_CONFIG, as user environment variables so that every process gets them, to paths outside AppData, for example %USERPROFILE%\\.heli\\state and %USERPROFILE%\\.heli\\config.toml.",
        );
    }
    let client = env!("CARGO_PKG_VERSION");
    if let Some(version) = &inputs.daemon_version
        && crate::status::older(version, client)
    {
        add(
            "daemon-version",
            "warn",
            format!(
                "the heli daemon is cargo-heli {version}, older than this cargo-heli ({client}); runs go through the old one until it is replaced."
            ),
            "Run `cargo heli restart`: it waits for the runs in flight (`--now` cancels them).",
        );
    }
    for version in &inputs.failed_installs {
        add(
            "install-failed",
            "warn",
            format!(
                "`cargo install` of cargo-heli {version} left no program: the Cargo home's .crates.toml lists it with none. A running cargo-heli cannot be overwritten on Windows."
            ),
            "Run `cargo heli self-upgrade`: it installs beside the running program and swaps it in by renaming.",
        );
    }
    for old in &inputs.old_programs {
        add(
            "old-program",
            "note",
            format!(
                "{} is a previous version `cargo heli self-upgrade` put aside and could not delete yet (something still runs it).",
                old.display()
            ),
            "Delete it once nothing runs it; the next `cargo heli self-upgrade` tries again.",
        );
    }
    Report {
        schema: SCHEMA,
        tools,
        findings,
        unknown_tools: unknown_usage(
            &inputs.events,
            &inputs.profiles,
            &inputs.repository_profiles,
        ),
    }
}

/// `cargo heli doctor`.
pub fn main(json: bool) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let report = diagnose(&gather(&cwd));
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a report is JSON")
        );
        return;
    }
    println!("Cargo tools:");
    if report.tools.is_empty() {
        println!("  none found");
    }
    for tool in &report.tools {
        println!(
            "  {:<12} {}",
            tool.name,
            tool.profile.unwrap_or("unknown: runs as exec")
        );
    }
    println!("\nFindings:");
    if report.findings.is_empty() {
        println!("  nothing to report");
    }
    for finding in &report.findings {
        println!(
            "  [{}] {}\n         -> {}",
            finding.level, finding.message, finding.action
        );
    }
    if !report.unknown_tools.is_empty() {
        println!(
            "\nTools without a profile, most used (write one under [profiles] or in .config/heli.toml, or contribute it to heli):"
        );
        for usage in &report.unknown_tools {
            println!(
                "  {:<12} {} run{}",
                usage.name,
                usage.runs,
                if usage.runs == 1 { "" } else { "s" }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> (PathBuf, toml::Table) {
        (PathBuf::from("config.toml"), text.parse().expect("toml"))
    }

    fn ids(report: &Report) -> Vec<&'static str> {
        report.findings.iter().map(|finding| finding.id).collect()
    }

    #[test]
    fn explains_jobs_target_dir_wrapper_and_linker() {
        let inputs = Inputs {
            configs: vec![config(
                "[build]\njobs = 16\ntarget-dir = \"D:/t\"\nrustc-wrapper = \"sccache\"\n\
                 [target.x86_64-unknown-linux-gnu]\nrustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n",
            )],
            ..Inputs::default()
        };
        assert_eq!(
            ids(&diagnose(&inputs)),
            ["jobs", "target-dir", "rustc-wrapper", "linker"]
        );
    }

    /// An older daemon, an install that left no program and an old program
    /// put aside are each reported; a daemon of this version is not.
    #[test]
    fn an_old_daemon_a_failed_install_and_an_old_program_are_reported() {
        let inputs = Inputs {
            daemon_version: Some("0.0.1".to_owned()),
            failed_installs: vec!["0.0.5".to_owned()],
            old_programs: vec![PathBuf::from("cargo-heli.exe.old-0.0.4")],
            ..Inputs::default()
        };
        let report = diagnose(&inputs);
        assert_eq!(
            ids(&report),
            ["daemon-version", "install-failed", "old-program"]
        );
        assert!(
            report.findings[0].message.contains("cargo-heli 0.0.1"),
            "{:?}",
            report.findings[0]
        );
        let current = Inputs {
            daemon_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            ..Inputs::default()
        };
        assert!(diagnose(&current).findings.is_empty());
        assert_eq!(
            failed_installs(
                "[v1]\n\"cargo-heli 0.0.4 (registry+x)\" = [\"cargo-heli.exe\"]\n\
                 \"cargo-heli 0.0.5 (registry+x)\" = []\n\"cargo-nextest 0.9.1 (registry+x)\" = []\n"
            ),
            ["0.0.5"]
        );
        assert_eq!(failed_installs("not toml"), Vec::<String>::new());
    }

    #[test]
    fn a_store_under_its_floor_is_reported() {
        let with = |free: u64| Inputs {
            store: Some(PathBuf::from("D:/heli-cache")),
            store_space: Some((free, 50 << 30)),
            ..Inputs::default()
        };
        let report = diagnose(&with(300 << 20));
        assert_eq!(ids(&report), ["store-unused", "store-floor"]);
        assert!(
            report.findings[1].message.contains("0.3 GiB free"),
            "{:?}",
            report.findings[1]
        );
        assert_eq!(ids(&diagnose(&with(60 << 30))), ["store-unused"]);
    }

    /// A pool saturated for most of the last hour, by the event log and
    /// the spell going on, is reported, and so are the tests nextest timed
    /// out meanwhile (#127).
    #[test]
    fn a_saturated_pool_and_its_timeouts_are_reported() {
        let now = 10_000;
        let spell = |since: u64, seconds: u64| {
            format!(
                r#"{{"at":{},"event":"saturated","since":{since},"seconds":{seconds},"slots":32,"most_active":18,"most_waiting":17}}"#,
                since + seconds
            )
        };
        let mut inputs = Inputs {
            events: vec![
                spell(1_000, 600),
                spell(now - 3_000, 1_200),
                spell(now - 1_500, 300),
            ],
            now,
            ..Inputs::default()
        };
        assert_eq!(
            saturation(&inputs.events, None, now).seconds,
            1_500,
            "the spell an hour ago is not counted"
        );
        assert_eq!(ids(&diagnose(&inputs)), Vec::<&str>::new());
        inputs.pool = Some((
            32,
            crate::status::Saturation {
                active_runs: 18,
                waiting_runs: 16,
                saturated_s: Some(600),
                wait_p50_ms: Some(45_000),
                wait_p90_ms: Some(336_000),
                window_s: 600,
            },
        ));
        inputs.events.push(format!(
            r#"{{"at":{},"event":"timeouts","run":"r9","tests":["facade facade_consumer_check"]}}"#,
            now - 60
        ));
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["pool-saturated", "nested-cargo-timeouts"]);
        assert!(
            report.findings[0]
                .message
                .contains("35 min of the last hour: up to 18 runs shared its 32 slots, 17 of them"),
            "{:?}",
            report.findings[0]
        );
        assert!(
            report.findings[1]
                .message
                .contains("facade facade_consumer_check"),
            "{:?}",
            report.findings[1]
        );
    }

    /// A budget the daemon's passes cannot meet is reported, with what
    /// holds it (#122).
    #[test]
    fn a_budget_the_daemon_cannot_meet_is_reported() {
        let inputs = Inputs {
            unmet_budget: Some(crate::status::UnmetBudget {
                budget: 100 << 30,
                used: 386 << 30,
                busy_dirs: 18,
                busy_bytes: 200 << 30,
                since: 0,
            }),
            ..Inputs::default()
        };
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["store-budget"]);
        assert!(
            report.findings[0]
                .message
                .contains("budget 100.0 GiB cannot be met: 18 busy directories hold 200.0 GiB"),
            "{:?}",
            report.findings[0]
        );
        assert!(
            report.findings[0].message.contains("386.0 GiB"),
            "{:?}",
            report.findings[0]
        );
    }

    #[test]
    fn makeflags_naming_another_jobserver_are_reported() {
        let ours = "-j --jobserver-auth=heli-09df93cc14834806";
        let with = |vars: &[(&str, &str)]| Inputs {
            env: vars
                .iter()
                .map(|(var, value)| ((*var).to_owned(), (*value).to_owned()))
                .collect(),
            makeflags: Some(ours.to_owned()),
            ..Inputs::default()
        };
        let stale = "-j --jobserver-auth=heli-3d99e7e1415767df";
        let report = diagnose(&with(&[("CARGO_MAKEFLAGS", stale)]));
        assert_eq!(ids(&report), ["makeflags"]);
        let finding = &report.findings[0];
        assert_eq!(finding.level, "warn");
        assert!(
            finding.message.contains("heli-3d99e7e1415767df"),
            "{finding:?}"
        );
        assert!(finding.action.contains(ours), "{finding:?}");
        // MAKEFLAGS counts when CARGO_MAKEFLAGS is not set, as for cargo.
        let report = diagnose(&with(&[("MAKEFLAGS", "-j4 --jobserver-fds=3,4")]));
        assert_eq!(ids(&report), ["makeflags"]);
        assert!(
            ids(&diagnose(&with(&[
                ("CARGO_MAKEFLAGS", ours),
                ("MAKEFLAGS", stale)
            ])))
            .is_empty()
        );
        // The pool itself, a run's pool in it, and flags without a jobserver.
        for flags in [ours, "-j --jobserver-auth=heli-09df93cc14834806-42", "-j4"] {
            assert!(
                ids(&diagnose(&with(&[("CARGO_MAKEFLAGS", flags)]))).is_empty(),
                "{flags}"
            );
        }
        assert!(
            !ids(&diagnose(&with(&[(
                "CARGO_MAKEFLAGS",
                "-j --jobserver-auth=heli-09df93cc148348060"
            )])))
            .is_empty(),
            "another pool whose name starts with this one's"
        );
    }

    #[test]
    fn a_users_own_build_dir_is_left_alone() {
        let inputs = Inputs {
            configs: vec![config("[build]\nbuild-dir = \"E:/b\"\n")],
            ..Inputs::default()
        };
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["build-dir"]);
        assert!(report.findings[0].message.contains("your own"));
    }

    #[test]
    fn a_name_outside_linker_settings_is_not_a_linker() {
        let inputs = Inputs {
            configs: vec![config("[alias]\nb = \"build --mold\"\n")],
            ..Inputs::default()
        };
        assert!(ids(&diagnose(&inputs)).is_empty());
    }

    #[test]
    fn a_broken_advisory_database_is_reported() {
        let home = tempfile::tempdir().expect("dir");
        let db = home.path().join("advisory-db");
        std::fs::create_dir_all(db.join("advisory-db-3157b0e258782691")).expect("dir");
        let inputs = Inputs {
            cargo_home: Some(home.path().to_owned()),
            ..Inputs::default()
        };
        assert_eq!(ids(&diagnose(&inputs)), ["advisory-db"]);
        std::fs::create_dir_all(db.join(".git")).expect("dir");
        assert!(ids(&diagnose(&inputs)).is_empty());
    }

    #[test]
    fn counts_the_runs_of_unknown_tools() {
        let line = |event: &str, argv: &str, profile: &str| {
            format!(r#"{{"at":1,"event":"{event}","argv":{argv},"profile":"{profile}"}}"#)
        };
        let events = vec![
            line("admit", r#"["cargo","bloat"]"#, "unknown"),
            line(
                "direct",
                r#"["cargo","+nightly","bloat","--release"]"#,
                "unknown",
            ),
            line("admit", r#"["cargo","expand"]"#, "unknown"),
            line("admit", r#"["cargo","build"]"#, "cargo"),
            line("admit", r#"["cargo","xtask"]"#, "user"),
            line("evict", r#"[]"#, "unknown"),
            "not json".to_owned(),
        ];
        let report = diagnose(&Inputs {
            events,
            ..Inputs::default()
        });
        assert_eq!(
            report.unknown_tools,
            [
                Usage {
                    name: "bloat".into(),
                    runs: 2
                },
                Usage {
                    name: "expand".into(),
                    runs: 1
                },
            ]
        );
    }

    #[test]
    fn leaves_out_tools_that_have_a_profile_now() {
        let line =
            |argv: &str| format!(r#"{{"at":1,"event":"admit","argv":{argv},"profile":"unknown"}}"#);
        let events = vec![
            line(r#"["cargo","binstall","x"]"#),
            line(r#"["cargo","bloat"]"#),
            line(r#"["cargo","fetch"]"#),
            line(r#"["cargo","xtask"]"#),
            line(r#"["cargo","just"]"#),
        ];
        let rule = |tool: &str| Rule {
            tool: tool.to_owned(),
            args: Vec::new(),
            class: crate::profiles::RuleClass::Exec,
        };
        assert!(class::tool_profile("binstall").is_some());
        let report = diagnose(&Inputs {
            events,
            profiles: Profiles {
                tool: vec![rule("xtask")],
                ..Profiles::default()
            },
            repository_profiles: vec![rule("just")],
            ..Inputs::default()
        });
        assert_eq!(
            report.unknown_tools,
            [Usage {
                name: "bloat".into(),
                runs: 1
            }]
        );
    }

    #[test]
    fn a_redirected_location_is_reported() {
        let inputs = Inputs {
            redirections: vec![(
                "configuration",
                PathBuf::from(r"C:\Users\u\AppData\Roaming\heli\config.toml"),
                PathBuf::from(
                    r"C:\Users\u\AppData\Local\Packages\X\LocalCache\Roaming\heli\config.toml",
                ),
            )],
            ..Inputs::default()
        };
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["package-redirection"]);
        assert!(report.findings[0].action.contains("HELI_CONFIG"));
    }

    #[test]
    fn environment_overrides_count() {
        let mut inputs = Inputs::default();
        inputs.env.insert("CARGO_TARGET_DIR".into(), "/x".into());
        assert_eq!(ids(&diagnose(&inputs)), ["target-dir"]);
    }

    #[test]
    fn lists_tools_with_their_profiles() {
        let dir = tempfile::tempdir().expect("dir");
        let exe = std::env::consts::EXE_SUFFIX;
        for name in [
            format!("cargo-insta{exe}"),
            format!("cargo-bloat{exe}"),
            format!("cargo-heli{exe}"),
            "cargo-fmt.pdb".to_owned(),
            "other".to_owned(),
        ] {
            std::fs::write(dir.path().join(name), "").expect("file");
        }
        let report = diagnose(&Inputs {
            bin_dirs: vec![dir.path().to_owned()],
            ..Inputs::default()
        });
        let names: Vec<_> = report
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.profile.is_some()))
            .collect();
        assert_eq!(names, [("bloat", false), ("insta", true)]);
    }

    fn installed(names: &[&str]) -> (tempfile::TempDir, Inputs) {
        let dir = tempfile::tempdir().expect("dir");
        let exe = std::env::consts::EXE_SUFFIX;
        for name in names {
            std::fs::write(dir.path().join(format!("cargo-{name}{exe}")), "").expect("file");
        }
        let inputs = Inputs {
            bin_dirs: vec![dir.path().to_owned()],
            ..Inputs::default()
        };
        (dir, inputs)
    }

    #[test]
    fn nextest_llvm_cov_and_mutants_show_what_heli_does() {
        let (_dir, inputs) = installed(&["nextest", "llvm-cov", "mutants"]);
        let report = diagnose(&inputs);
        assert_eq!(report.tools.len(), 3);
        for tool in &report.tools {
            assert!(tool.profile.is_some(), "{} has no profile", tool.name);
        }
        let nextest = report.tools.iter().find(|tool| tool.name == "nextest");
        assert!(nextest.unwrap().profile.unwrap().contains("test slots"));
    }

    #[test]
    fn helpers_of_an_installed_multi_binary_tool_are_not_listed() {
        let (_dir, inputs) = installed(&["ndk", "ndk-env", "ndk-runner", "ndk-test", "zigbuild"]);
        let names: Vec<_> = diagnose(&inputs)
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, ["ndk", "zigbuild"]);
        let (_dir, inputs) = installed(&["ndk-env"]);
        assert_eq!(diagnose(&inputs).tools.len(), 1);
    }

    #[test]
    fn the_home_finding_follows_the_configured_mode() {
        let mut inputs = Inputs::default();
        inputs.env.insert("CARGO_HOME".into(), "D:/ch".into());
        let report = diagnose(&inputs);
        assert!(report.findings[0].message.contains("shared"));
        inputs.home_mode = CargoHome::Isolated;
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["cargo-home"]);
        assert!(report.findings[0].message.contains("private Cargo home"));
        assert!(report.findings[0].message.contains("original home"));
    }

    #[test]
    fn a_store_the_workspace_does_not_use_is_reported() {
        let store = PathBuf::from(if cfg!(windows) { r"D:\cache" } else { "/cache" });
        let mut inputs = Inputs {
            store: Some(store.clone()),
            ..Inputs::default()
        };
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["store-unused"]);
        assert!(report.findings[0].action.contains("{workspace-path-hash}"));
        let line = crate::setup::build_dir_line(&store);
        assert!(report.findings[0].action.contains(&format!("`{line}`")));
        if cfg!(windows) {
            assert!(!line.contains('/'), "{line}");
        }
        assert!(report.findings[0].action.contains("cargo heli setup"));
        inputs.configs = vec![config(
            "[build]
build-dir = \"D:/cache/{workspace-path-hash}\"
",
        )];
        assert_eq!(ids(&diagnose(&inputs)), ["build-dir"]);
        inputs.configs.clear();
        inputs
            .env
            .insert("CARGO_BUILD_BUILD_DIR".into(), "D:/b".into());
        assert_eq!(ids(&diagnose(&inputs)), ["build-dir"]);
    }

    #[test]
    fn a_fixed_test_thread_count_is_reported() {
        let mut inputs = Inputs::default();
        inputs.env.insert("RUST_TEST_THREADS".into(), "16".into());
        assert_eq!(ids(&diagnose(&inputs)), ["test-threads"]);
        let inputs = Inputs {
            configs: vec![config(
                "[env]
RUST_TEST_THREADS = \"16\"
",
            )],
            ..Inputs::default()
        };
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["test-threads"]);
        assert!(report.findings[0].message.contains("= 16"));
        let inputs = Inputs {
            configs: vec![config(
                "[env]
RUST_TEST_THREADS = { value = \"8\", force = true }
",
            )],
            ..Inputs::default()
        };
        assert_eq!(ids(&diagnose(&inputs)), ["test-threads"]);
    }
}
