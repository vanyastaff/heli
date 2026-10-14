//! The environment heli hands on: its own, with some variables replaced;
//! and the rules every part of heli reads an environment by.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};

/// Variables a shell sets for its own bookkeeping, which differ between two
/// shells of one agent and which no build reads.
pub const SHELL_BOOKKEEPING: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];

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

/// `program args` as a run would start it, to ask it something: in `dir`,
/// with exactly the environment `env`, with no input, its stdout captured
/// and its stderr dropped.
pub fn command<K, V>(
    program: impl AsRef<OsStr>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    dir: &Path,
    env: impl IntoIterator<Item = (K, V)>,
) -> Command
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// Whether the variable names `name` and `other` name one variable: on
/// Windows without case, as there.
#[cfg(windows)]
pub fn same_name(name: &OsStr, other: &str) -> bool {
    name.eq_ignore_ascii_case(other)
}

/// Whether the variable names `name` and `other` name one variable: on
/// Windows without case, as there.
#[cfg(not(windows))]
pub fn same_name(name: &OsStr, other: &str) -> bool {
    name == other
}

/// `name` spelled so that two names of one variable are equal: upper-cased
/// on Windows, where names ignore case, and as it is elsewhere.
pub fn folded(name: &OsStr) -> OsString {
    if cfg!(windows) {
        name.to_ascii_uppercase()
    } else {
        name.to_os_string()
    }
}
