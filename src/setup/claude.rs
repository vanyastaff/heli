//! Claude Code's user settings: the variable goes into their `env` block,
//! which Claude Code sets for every command its agents run.
//!
//! The file is `settings.json` in Claude Code's configuration directory:
//! `CLAUDE_CONFIG_DIR`, else `.claude` in the home (on Windows
//! `USERPROFILE`, where Node's `os.homedir()` finds it). It is offered only
//! when that directory exists, that is when Claude Code is set up for this
//! user. setup rewrites the whole file, as Claude Code does: JSON with
//! two-space indentation, every other setting kept, in its order.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{Edit, Shown, VAR};

/// The settings file, or why there is none to offer.
pub fn settings() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => super::home(&["USERPROFILE"])
            .ok_or("Claude Code settings: no home directory to look in.")?
            .join(".claude"),
    };
    if dir.is_dir() {
        Ok(dir.join("settings.json"))
    } else {
        Err(format!(
            "Claude Code settings: none here ({} does not exist).",
            dir.display()
        ))
    }
}

/// The edit that sets `env.CARGO_MAKEFLAGS` to `value` in the settings at
/// `path`; a missing file counts as empty settings.
pub fn edit(path: &Path, value: &str) -> Edit {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Edit::Refused(format!("cannot read it ({error})")),
    };
    let mut settings = if text.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str(&text) {
            Ok(Value::Object(settings)) => settings,
            Ok(_) => return Edit::Refused("it is not a JSON object".to_owned()),
            Err(error) => return Edit::Refused(format!("it is not JSON ({error})")),
        }
    };
    let env = settings
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(env) = env else {
        return Edit::Refused("its `env` is not a JSON object".to_owned());
    };
    if env.get(VAR).and_then(Value::as_str) == Some(value) {
        return Edit::Done;
    }
    env.insert(VAR.to_owned(), Value::String(value.to_owned()));
    let mut content =
        serde_json::to_string_pretty(&settings).expect("BUG: a JSON object serializes");
    content.push('\n');
    Edit::Write {
        content,
        shown: Shown::Whole,
    }
}
