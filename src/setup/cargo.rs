//! Cargo's configuration: `build.build-dir` points at heli's build store
//! (`crate::store`), so cargo keeps a workspace's intermediates there and
//! only the final artifacts in its `target/`.
//!
//! Two files are offered, each with its own question:
//!
//! - **This repository**: `.cargo/config.toml` in its main worktree (the one
//!   holding the git common directory, else the worktree setup runs in).
//!   Cargo reads the `.cargo/` of every directory above the one it runs in,
//!   so it covers the main worktree and every worktree inside it (as Claude
//!   Code puts them, in `.claude/worktrees/`), and nothing else. Its store
//!   is the one on that worktree's volume. If the file is tracked, the line
//!   shows as a local change, which is this machine's and not to be
//!   committed.
//! - **Every project**: the user's Cargo configuration, `config.toml` in the
//!   Cargo home (`CARGO_HOME`, else `.cargo` in the home). One value serves
//!   every volume, so its store is the one on the volume setup runs in.
//!
//! Where a directory holds only the older `config`, that file is edited.
//!
//! **The edit.** setup adds two lines, a comment that marks them as heli's
//! and `build-dir = "<store>/{workspace-path-hash}"`, under the file's
//! `[build]` table, or in a new one at its end. Lines an earlier setup wrote
//! with another value are replaced. The new content must parse to the same
//! configuration plus `build.build-dir`; otherwise, and when the file is not
//! TOML or sets `build.build-dir` itself, setup leaves it as it is, with the
//! reason.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Edit, Shown};

/// The setting this file manages, as setup names it.
pub const SETTING: &str = "build.build-dir";

/// The comment that marks heli's line.
const MARKER: &str =
    "# Added by `cargo heli setup`: cargo keeps its intermediates in heli's build store.";

/// The key under `[build]`.
pub const KEY: &str = "build-dir";

/// The main worktree of the repository holding `dir`, or why there is none.
pub fn repository(dir: &Path) -> Result<PathBuf, String> {
    let none = || {
        format!(
            "Cargo configuration for this repository: none ({} is not in a git repository).",
            dir.display()
        )
    };
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--show-toplevel",
        ])
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| none())?;
    if !output.status.success() {
        return Err(none());
    }
    let text = String::from_utf8(output.stdout).map_err(|_| none())?;
    let mut lines = text.lines();
    let (Some(common), Some(toplevel)) = (lines.next(), lines.next()) else {
        return Err(none());
    };
    // git writes `/` between components on Windows too.
    let native = |path: &str| Path::new(path).components().collect::<PathBuf>();
    let common = native(common);
    Ok(match common.parent() {
        Some(main) if common.file_name().is_some_and(|name| name == ".git") => main.to_path_buf(),
        _ => native(toplevel),
    })
}

/// The user's Cargo home: `CARGO_HOME` (relative to `cwd`), else `.cargo`
/// in the home.
pub fn home(cwd: &Path) -> Option<PathBuf> {
    match std::env::var_os("CARGO_HOME").filter(|home| !home.is_empty()) {
        Some(home) => Some(cwd.join(home)),
        None => super::home(&["USERPROFILE"]).map(|home| home.join(".cargo")),
    }
}

/// The configuration file cargo reads in `dir`: `config.toml`, or the older
/// `config` when only that exists.
pub fn file(dir: &Path) -> PathBuf {
    let toml = dir.join("config.toml");
    let old = dir.join("config");
    if !toml.exists() && old.is_file() {
        old
    } else {
        toml
    }
}

/// The edit that makes the configuration at `path` set `build.build-dir` to
/// `value`; a missing file counts as empty.
pub fn edit(path: &Path, value: &str) -> Edit {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Edit::Refused(format!("cannot read it ({error})")),
    };
    let Ok(table) = parse(&text) else {
        return Edit::Refused("it is not TOML".to_owned());
    };
    if build_dir(&table) == Some(&toml::Value::String(value.to_owned())) {
        return Edit::Done;
    }
    let unsafe_edit = || {
        Edit::Refused(format!(
            "setup cannot add {SETTING} to it safely; add `{KEY} = {}` under [build] by hand",
            basic_string(value)
        ))
    };
    let kept = without_ours(&text);
    // What does not parse without heli's lines fails the check below.
    if let Some(theirs) = build_dir(&parse(&kept).unwrap_or_default()) {
        return Edit::Refused(match theirs {
            toml::Value::String(theirs) => {
                format!("it sets {SETTING} itself, to {}", basic_string(theirs))
            }
            _ => format!("it sets {SETTING} itself"),
        });
    }
    let content = insert(&kept, value);
    // The new content must say what the file said, plus heli's value.
    let sound = parse(&content).is_ok_and(|new| {
        build_dir(&new) == Some(&toml::Value::String(value.to_owned()))
            && without_build_dir(new) == without_build_dir(table)
    });
    if !sound {
        return unsafe_edit();
    }
    Edit::Write {
        content,
        shown: Shown::Whole,
    }
}

fn parse(text: &str) -> Result<toml::Table, toml::de::Error> {
    toml::from_str(text)
}

/// The value of `build.build-dir` in `table`, if it sets one.
fn build_dir(table: &toml::Table) -> Option<&toml::Value> {
    table.get("build")?.as_table()?.get(KEY)
}

/// `table` without `build.build-dir`, and without a `[build]` left empty.
fn without_build_dir(mut table: toml::Table) -> toml::Table {
    if let Some(toml::Value::Table(build)) = table.get_mut("build") {
        build.remove(KEY);
        if build.is_empty() {
            table.remove("build");
        }
    }
    table
}

/// `text` without the lines an earlier setup wrote: its comment and the
/// `build-dir` line after it.
fn without_ours(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let ours = line.trim_end() == MARKER
            && lines
                .peek()
                .is_some_and(|next| is_build_dir_line(next.trim_start()));
        if ours {
            lines.next();
        } else {
            kept.push_str(line);
        }
    }
    kept
}

fn is_build_dir_line(line: &str) -> bool {
    line.strip_prefix(KEY)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// `text` with heli's lines under its first `[build]` header, or in a new
/// `[build]` table at its end.
fn insert(text: &str, value: &str) -> String {
    let ours = format!("{MARKER}\n{KEY} = {}\n", basic_string(value));
    let mut content = String::with_capacity(text.len() + ours.len() + 10);
    let mut inserted = false;
    for line in text.split_inclusive('\n') {
        content.push_str(line);
        if !inserted && is_build_header(line) {
            if !line.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&ours);
            inserted = true;
        }
    }
    if !inserted {
        if !content.is_empty() {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push('\n');
        }
        content.push_str("[build]\n");
        content.push_str(&ours);
    }
    content
}

/// Whether `line` opens the `[build]` table.
fn is_build_header(line: &str) -> bool {
    line.trim()
        .strip_prefix("[build]")
        .is_some_and(|rest| rest.trim_start().is_empty() || rest.trim_start().starts_with('#'))
}

/// `value` as a TOML basic string.
pub fn basic_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            control if control.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", u32::from(control)));
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}
