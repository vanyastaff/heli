//! The environment heli hands on: its own, with some variables replaced.

use std::ffi::{OsStr, OsString};

/// This process's environment, with each of `overrides` set (replacing a
/// variable of the same name; names ignore case on Windows, as there).
pub fn current_with(overrides: &[(&str, OsString)]) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| {
            !overrides
                .iter()
                .any(|(overridden, _)| same_name(name, overridden))
        })
        .collect();
    env.extend(
        overrides
            .iter()
            .map(|(name, value)| (OsString::from(name), value.clone())),
    );
    env
}

#[cfg(windows)]
fn same_name(name: &OsStr, other: &str) -> bool {
    name.eq_ignore_ascii_case(other)
}

#[cfg(not(windows))]
fn same_name(name: &OsStr, other: &str) -> bool {
    name == other
}
