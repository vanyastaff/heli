//! `cargo heli doctor [--json]`: how the installed cargo tools and the
//! cargo configuration interact with heli (spec #38).
//!
//! It reads, and changes nothing: the `cargo-*` programs on the search path
//! and in the Cargo home's `bin`, the Cargo configuration cargo would read
//! here (the Cargo home's, and every `.cargo/` from the working directory
//! up), and the variables that override it. Each finding says what happens
//! and what to do about it.
//!
//! **`--json`**, a versioned document (`schema`, now 1) whose fields are only
//! added:
//!
//! ```json
//! {"schema":1,
//!  "tools":[{"name":"insta","profile":"accept/reject mutate …"},
//!           {"name":"zigbuild","profile":null}],
//!  "findings":[{"id":"jobs","level":"note","message":"…","action":"…"}]}
//! ```
//!
//! `profile` is `null` for a tool heli has no built-in profile for. `level`
//! is `note` (how heli behaves) or `warn` (something the user likely
//! expects to work and will not).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::class;

/// The version of the `--json` document.
const SCHEMA: u32 = 1;

/// Variables that override the Cargo configuration doctor explains.
const VARS: [&str; 6] = [
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
    "RUSTC_WRAPPER",
    "CARGO_HOME",
];

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

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: u32,
    pub tools: Vec<Tool>,
    pub findings: Vec<Finding>,
}

/// Reads the real environment, from `cwd`.
pub fn gather(cwd: &Path) -> Inputs {
    let mut inputs = Inputs::default();
    if let Some(path) = std::env::var_os("PATH") {
        inputs.bin_dirs.extend(std::env::split_paths(&path));
    }
    let home = crate::setup::cargo_home(cwd);
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
            let stem = file.strip_suffix(".exe").unwrap_or(&file);
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
    let tools: Vec<Tool> = installed_tools(&inputs.bin_dirs)
        .into_iter()
        .map(|name| Tool {
            profile: class::tool_profile(&name),
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
        add(
            "cargo-home",
            "note",
            format!(
                "CARGO_HOME is {home}; `--cargo-home isolated` gives managed runs a private home per worktree instead."
            ),
            "Nothing to do.",
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
    Report {
        schema: SCHEMA,
        tools,
        findings,
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
    fn environment_overrides_count() {
        let mut inputs = Inputs::default();
        inputs.env.insert("CARGO_TARGET_DIR".into(), "/x".into());
        assert_eq!(ids(&diagnose(&inputs)), ["target-dir"]);
    }

    #[test]
    fn lists_tools_with_their_profiles() {
        let dir = tempfile::tempdir().expect("dir");
        for name in ["cargo-insta.exe", "cargo-zigbuild", "cargo-heli", "other"] {
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
        assert_eq!(names, [("insta", true), ("zigbuild", false)]);
    }
}
