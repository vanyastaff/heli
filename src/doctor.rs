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

/// The version of the `--json` document.
const SCHEMA: u32 = 1;

/// Variables that override the Cargo configuration doctor explains.
const VARS: [&str; 7] = [
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
    /// `[cargo] home` of heli's configuration.
    pub home_mode: CargoHome,
    /// The build store that serves the working directory, when it exists:
    /// a configured `[store] paths` entry or the volume's default store.
    pub store: Option<PathBuf>,
    /// The event log's lines, oldest first (the rotated file, then the
    /// current one).
    pub events: Vec<String>,
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

/// The tools of the runs in `events` that no profile named, by runs, most
/// first (then by name), [`UNKNOWN_SHOWN`] at most.
fn unknown_usage(events: &[String]) -> Vec<Usage> {
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
        if let Some(tool) = tool {
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
    }
    if let Ok(locations) = crate::locations::Locations::resolve() {
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
    for var in VARS {
        if let Some(value) = std::env::var_os(var).filter(|value| !value.is_empty()) {
            inputs
                .env
                .insert(var.to_owned(), value.to_string_lossy().into_owned());
        }
    }
    inputs
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
                "A global target directory is set ({dir}). Workspaces sharing it share one build directory, so cargo's artifact lock serializes them and the build store's recycling and eviction cannot own it."
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
        let store = store.display();
        add(
            "store-unused",
            "warn",
            format!(
                "The build store {store} exists, but no Cargo configuration reaching this workspace sets build.build-dir, so its intermediates stay in target/ and the store is unused."
            ),
            &format!(
                "Run `cargo heli setup`, or add `build-dir = '{store}/{{workspace-path-hash}}'` under [build] in a Cargo configuration (or set CARGO_BUILD_BUILD_DIR)."
            ),
        );
    }
    if let Some(threads) =
        env("RUST_TEST_THREADS").or_else(|| env_table_test_threads(&inputs.configs))
    {
        add(
            "test-threads",
            "note",
            format!(
                "RUST_TEST_THREADS = {threads}: in a test run, heli's test slots decide the threads. `cargo heli test` passes `--test-threads=K` after `--`, which overrides the variable, and K is at most the variable's value when the client's environment has it (a Cargo [env] entry is not seen by heli, so it only gets overridden). `cargo heli nextest run` passes `--test-threads K` and ignores the variable. Other commands keep it."
            ),
            "Nothing to do; to bound the threads of a run, pass --test-threads (or -j for nextest) or size the pool.",
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
    Report {
        schema: SCHEMA,
        tools,
        findings,
        unknown_tools: unknown_usage(&inputs.events),
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
        let mut inputs = Inputs {
            store: Some(PathBuf::from("D:/cache")),
            ..Inputs::default()
        };
        let report = diagnose(&inputs);
        assert_eq!(ids(&report), ["store-unused"]);
        assert!(report.findings[0].action.contains("{workspace-path-hash}"));
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
