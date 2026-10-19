//! The repository's git `post-checkout` hook (docs/build-store.md#recycling):
//! git runs it after every checkout, and `git worktree add` checks the new
//! worktree out, from the null commit. Then the hook runs `cargo-heli recycle`
//! (`crate::recycle`) in the new worktree, which gets a removed worktree's
//! build directory before anything builds in it. Any other checkout, whose
//! first argument is a real commit, does nothing.
//!
//! The hook is the file `post-checkout` in the repository's hooks
//! directory, as git finds it (`git rev-parse --git-path hooks`, which
//! follows `core.hooksPath`): one for every worktree of the repository. It
//! names heli by its full path, as the program that wrote it (should heli
//! move, the shell says it is not found, and setup run again rewrites the
//! hook), and never fails a checkout: it exits 0 whatever heli does. A hook
//! that setup did
//! not write (it lacks heli's marker line) is left as it is, with the
//! reason; one an earlier setup wrote is replaced when it differs.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::Edit;

/// What the hook sets, as setup names it.
pub const SETTING: &str = "build directory recycling";

/// The line that marks a hook as heli's.
const MARKER: &str =
    "# Added by `cargo heli setup`: a new worktree gets a removed worktree's build directory.";

/// The `post-checkout` hook of the repository holding `dir`, or why there
/// is none.
pub fn path(dir: &Path) -> Result<PathBuf, String> {
    let none = || {
        format!(
            "Git post-checkout hook: none ({} is not in a git repository).",
            dir.display()
        )
    };
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "hooks/post-checkout",
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
    // git writes `/` between components on Windows too.
    Ok(Path::new(text.trim_end()).components().collect())
}

/// The edit that makes the hook at `path` run `program`, heli.
pub fn edit(path: &Path, program: &Path) -> Edit {
    let content = match script(program) {
        Some(content) => content,
        None => {
            return Edit::Refused(format!("heli's path {} is not Unicode", program.display()));
        }
    };
    match std::fs::read_to_string(path) {
        Ok(text) if text == content => Edit::Done,
        Ok(text) if text.lines().any(|line| line == MARKER) => Edit::Script { content },
        Ok(_) => Edit::Refused(format!(
            "it is not heli's; to recycle build directories, have it run \
             `{} recycle` when its first argument is all zeros",
            quoted(program).unwrap_or_default()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Edit::Script { content },
        Err(error) => Edit::Refused(format!("cannot read it ({error})")),
    }
}

/// The hook that runs `program`, if its path can be quoted.
fn script(program: &Path) -> Option<String> {
    let program = quoted(program)?;
    Some(format!(
        "#!/bin/sh\n\
         {MARKER}\n\
         # git gives the commit it checked out from first: all zeros when\n\
         # `git worktree add` checks a new worktree out.\n\
         case \"$1\" in\n\
         *[!0]* | \"\") exit 0 ;;\n\
         esac\n\
         {program} recycle </dev/null >/dev/null\n\
         exit 0\n"
    ))
}

/// `program` as a word of a shell script, in single quotes (Git for
/// Windows' shell reads a Windows path so too), if it is Unicode.
fn quoted(program: &Path) -> Option<String> {
    let text = program.to_str()?;
    Some(format!("'{}'", text.replace('\'', r"'\''")))
}
