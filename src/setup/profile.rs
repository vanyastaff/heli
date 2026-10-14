//! The shell profile: setup adds two lines at its end, a comment that marks
//! them as heli's and the assignment, in the syntax of the user's shell.
//!
//! The shell is the one `SHELL` names (on Windows, Git Bash, MSYS2 and
//! Cygwin set it), and its profile the file an interactive shell reads:
//!
//! | `SHELL` | profile | syntax |
//! |---|---|---|
//! | bash | `~/.bashrc` | `export CARGO_MAKEFLAGS='…'` |
//! | zsh | `$ZDOTDIR/.zshrc`, else `~/.zshrc` | `export CARGO_MAKEFLAGS='…'` |
//! | fish | `$XDG_CONFIG_HOME/fish/config.fish`, else `~/.config/fish/config.fish` | `set -gx CARGO_MAKEFLAGS '…'` |
//! | any other | `~/.profile` | `export CARGO_MAKEFLAGS='…'` |
//!
//! `~` is `HOME`, else on Windows `USERPROFILE`. Without `SHELL` (PowerShell
//! and cmd on Windows), no profile is offered: setup says how to set the
//! variable in the user's environment instead.
//!
//! Lines an earlier setup wrote with another value are replaced: removed
//! where they are, and the new ones added at the end.

use std::io;
use std::path::{Path, PathBuf};

use super::{Edit, Shown, VAR};

/// The comment that marks heli's lines.
const MARKER: &str = "# Added by `cargo heli setup`: a bare cargo draws from heli's host pool.";

/// How a shell sets an exported variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// sh, bash, zsh and the like: `export NAME='value'`.
    Sh,
    /// fish: `set -gx NAME 'value'`.
    Fish,
}

impl Syntax {
    /// The line that sets [`VAR`] to `value`.
    fn assignment(self, value: &str) -> String {
        match self {
            Self::Sh => format!("export {VAR}='{}'", value.replace('\'', r"'\''")),
            Self::Fish => format!(
                "set -gx {VAR} '{}'",
                value.replace('\\', r"\\").replace('\'', r"\'")
            ),
        }
    }
}

/// The profile of the shell `SHELL` names, and its syntax; `None` without
/// `SHELL` or a home.
pub fn locate() -> Option<(PathBuf, Syntax)> {
    let shell = std::env::var_os("SHELL").filter(|shell| !shell.is_empty())?;
    let name = Path::new(&shell)
        .file_stem()?
        .to_string_lossy()
        .to_ascii_lowercase();
    let home = super::home(&["HOME", "USERPROFILE"])?;
    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
    };
    Some(match name.as_str() {
        "bash" => (home.join(".bashrc"), Syntax::Sh),
        "zsh" => (
            absolute("ZDOTDIR").unwrap_or(home).join(".zshrc"),
            Syntax::Sh,
        ),
        "fish" => (
            absolute("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home.join(".config"))
                .join("fish")
                .join("config.fish"),
            Syntax::Fish,
        ),
        _ => (home.join(".profile"), Syntax::Sh),
    })
}

/// What to do without a profile: set the variable by hand.
pub fn hint(value: &str) -> String {
    if cfg!(windows) {
        format!(
            "Shell profile: none (SHELL is not set). PowerShell and cmd read the user \
             environment; to set it there, run in PowerShell:\n\
             [Environment]::SetEnvironmentVariable('{VAR}', '{}', 'User')",
            value.replace('\'', "''")
        )
    } else {
        format!(
            "Shell profile: none (SHELL is not set). Add this to your shell's profile:\n{}",
            Syntax::Sh.assignment(value)
        )
    }
}

/// The edit that makes the profile at `path` set [`VAR`] to `value`; a
/// missing file counts as empty.
pub fn edit(path: &Path, syntax: Syntax, value: &str) -> Edit {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Edit::Refused(format!("cannot read it ({error})")),
    };
    let assignment = syntax.assignment(value);
    let mut kept = String::with_capacity(text.len());
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let heli = line.trim_end() == MARKER
            && lines
                .peek()
                .is_some_and(|next| is_assignment(next.trim_end()));
        if !heli {
            kept.push_str(line);
            continue;
        }
        let set = lines.next().unwrap_or_default();
        if set.trim_end() == assignment {
            return Edit::Done;
        }
    }
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push('\n');
    }
    let ending = format!("{MARKER}\n{assignment}\n");
    kept.push_str(&ending);
    Edit::Write {
        content: kept,
        shown: Shown::Ending(ending),
    }
}

/// Whether `line` assigns [`VAR`], in any syntax setup writes.
fn is_assignment(line: &str) -> bool {
    [format!("export {VAR}="), format!("set -gx {VAR} ")]
        .iter()
        .any(|start| line.starts_with(start.as_str()))
}
