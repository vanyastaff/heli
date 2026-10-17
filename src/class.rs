//! Run classes (`DESIGN.md` §5, "Classes"): what kind of work a run does,
//! read from its command line, and so what heli gives it.
//!
//! | Class   | Subcommands                 | Run pool | Test slots | Result store | Worktree lock |
//! |---------|-----------------------------|----------|------------|--------------|---------------|
//! | compile | build, run, doc             | yes      | no         | no           | shared        |
//! | verify  | check, clippy, fmt --check  | yes      | no         | yes          | shared        |
//! | test    | test, nextest run           | yes      | yes        | yes          | shared        |
//! | mutate  | fmt, fix, clippy --fix      | no       | no         | no           | exclusive     |
//! | exec    | anything else               | yes      | no         | opt-in       | shared        |
//!
//! This module is the one place that reads a command line into a class
//! ([`Class::of_cargo`]); what each column means for a run lives with the
//! mechanism that applies it (the worktree lock: `crate::daemon::registry`).
//!
//! How `cargo heli <subcommand> [args…]` is read:
//! - The subcommand is the first argument, after a `+toolchain` if there is
//!   one. Cargo's built-in aliases count as their subcommands (`b`, `c`, `d`,
//!   `r`, `t`), and so does nextest's `r` for `nextest run`. Aliases a project
//!   defines in its Cargo configuration are not resolved: they are exec.
//! - `fmt` only checks, and is verify, when `--check` is among its
//!   arguments: cargo-fmt's own flag, or rustfmt's after `--`.
//! - `clippy` fixes, and is mutate, when `--fix` comes before `--` (what
//!   follows `--` is for the compiler).
//! - Any other subcommand is exec, and so is `cargo heli exec -- <command>`
//!   whatever the command, `cargo fmt` included: heli does not look inside.
//!   `exec --verify` explicitly declares a read-only verification instead;
//!   the CLI assigns its verify class without inspecting the wrapped command.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

/// What kind of work a run does (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Compile,
    Verify,
    Test,
    Mutate,
    Exec,
}

/// How a run holds the lock of its worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// With other shared holders: the run reads the sources.
    Shared,
    /// Alone: the run rewrites the sources.
    Exclusive,
}

/// The tool that runs a test run's tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// `cargo test` (`t`): libtest, its arguments after `--`.
    Libtest,
    /// `cargo nextest run` (`nextest r`).
    Nextest,
}

/// The class of the built-in profile for a cargo subcommand heli does not
/// run itself (`DESIGN.md` §5, "Tool profiles"): tools that rewrite the
/// worktree are mutate runs, so that they never run under a build; tools
/// that only read it and report are verify runs, shared when nothing changed.
/// Anything not listed is exec.
fn third_party(tool: &str, rest: &[OsString]) -> Class {
    let before_separator = || rest.iter().take_while(|arg| *arg != "--");
    let first_word = || first_word(rest);
    match tool {
        // Edit Cargo.toml / Cargo.lock (cargo's own, cargo-edit's, and
        // cargo-release, which also bumps versions and commits).
        "add" | "remove" | "rm" | "update" | "upgrade" | "set-version" | "release"
        | "autoinherit" => Class::Mutate,
        // cargo-sort sorts manifests, or only checks them.
        "sort" if before_separator().any(|arg| arg == "--check" || arg == "-c") => Class::Verify,
        "sort" => Class::Mutate,
        // Rewrite sources or generated files when asked to.
        "machete" | "shear" if before_separator().any(|arg| arg == "--fix") => Class::Mutate,
        "machete" | "shear" | "udeps" | "public-api" => Class::Verify,
        "hakari" => match first_word() {
            Some("generate" | "manage-deps") => Class::Mutate,
            Some("verify") => Class::Verify,
            _ => Class::Exec,
        },
        "insta" => match first_word() {
            Some("accept" | "reject") => Class::Mutate,
            _ => Class::Exec,
        },
        "sqlx" if first_word() == Some("prepare") => Class::Mutate,
        // Delete build output or manifests' dependencies by themselves.
        "sweep" | "clean-all" => Class::Mutate,
        // Advisory checks are verdicts on the tree and the advisory
        // database, whose commit is in the key; the daemon fetches the
        // database first for a run that would (`crate::daemon::advisory`).
        "audit" => audit_class(rest),
        "deny" if first_word() == Some("check") || deny_checks(rest) => Class::Verify,
        // cargo-hack runs check or clippy over feature combinations.
        "hack" if before_separator().any(|arg| arg == "check" || arg == "clippy") => Class::Verify,
        _ => Class::Exec,
    }
}

/// The class of `cargo audit <rest>`: a check of the lockfile is verify,
/// `fix` rewrites manifests (mutate), and `bin`, which scans binaries outside
/// the tree, or anything heli cannot read, is exec.
fn audit_class(rest: &[OsString]) -> Class {
    // cargo-audit's options that take a value.
    const VALUED: &[&str] = &[
        "--db",
        "-d",
        "--deny",
        "-D",
        "--ignore",
        "--target-arch",
        "--target-os",
        "--file",
        "-f",
        "--url",
        "-u",
        "--color",
        "-c",
        "--format",
        "-F",
    ];
    let mut args = rest.iter().take_while(|arg| *arg != "--");
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return Class::Exec;
        };
        if VALUED.contains(&arg) {
            args.next();
        } else if arg == "fix" {
            return Class::Mutate;
        } else if !arg.starts_with('-') {
            return Class::Exec;
        }
    }
    Class::Verify
}

/// Whether `cargo deny <rest>` runs `check`, after its global options.
fn deny_checks(rest: &[OsString]) -> bool {
    rest.iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "check")
}

/// Cargo's own subcommands that are not mutate, verify, compile or test runs:
/// they run in the daemon as exec whatever the terminal.
const CARGO_BUILTINS: &[&str] = &[
    "bench",
    "tree",
    "metadata",
    "install",
    "uninstall",
    "search",
    "publish",
    "package",
    "clean",
    "fetch",
    "vendor",
    "generate-lockfile",
    "locate-project",
    "pkgid",
    "rustc",
    "rustdoc",
    "init",
    "new",
    "config",
    "login",
    "logout",
    "owner",
    "yank",
    "report",
    "help",
    "version",
    "verify-project",
    "nextest",
];

/// Whether `tool` is one of cargo's own subcommands that run as exec.
pub fn is_cargo_builtin(tool: &str) -> bool {
    CARGO_BUILTINS.contains(&tool)
}

/// Whether `cargo <args>` runs in the client, but drawing from the host pool
/// (`DESIGN.md` §5, "Tool profiles"), instead of in the daemon: interactive
/// and long-running tools (`insta review`, `dx serve`, `trunk serve`,
/// `leptos watch|serve`, `tauri dev`, `cargo watch`), which
/// need a terminal and hold a slot for hours, and, when stdin is a terminal,
/// every other exec command of a tool heli has no profile for.
pub fn runs_directly(args: &[OsString], stdin_is_terminal: bool) -> bool {
    let Some((tool, rest)) = subcommand(args) else {
        return false;
    };
    let tool = tool.to_str().unwrap_or_default();
    let word = first_word(rest);
    match (tool, word) {
        ("insta", Some("review"))
        | ("dx" | "trunk", Some("serve"))
        | ("leptos", Some("watch" | "serve"))
        | ("tauri", Some("dev"))
        | ("watch", _) => return true,
        _ => {}
    }
    stdin_is_terminal
        && !tool.is_empty()
        && !tool.starts_with('-')
        && Class::of_cargo(args) == Class::Exec
        && !CARGO_BUILTINS.contains(&tool)
        && tool_profile(tool).is_none()
}

/// The first of `args` that is not a flag.
fn first_word(args: &[OsString]) -> Option<&str> {
    args.iter()
        .filter_map(|arg| arg.to_str())
        .find(|arg| !arg.starts_with('-'))
}

/// What heli's built-in profile does with the cargo tool `cargo-<name>`, for
/// `cargo heli doctor`; `None` for a tool heli has no profile for (it runs
/// as exec).
pub fn tool_profile(name: &str) -> Option<&'static str> {
    Some(match name {
        "add" | "remove" | "rm" | "update" | "upgrade" | "set-version" | "release"
        | "autoinherit" => "mutate: exclusive worktree lock",
        "sort" => "--check verify cached; else mutate (exclusive lock)",
        "watch" => "runs directly in the pool, never attached to",
        "leptos" => "watch/serve run directly in the pool; others exec",
        "tauri" => "dev runs directly in the pool; others exec",
        "outdated" | "semver-checks" | "vet" | "geiger" => {
            "exec: never cached, the result depends on the registry"
        }
        "hakari" => "generate/manage-deps mutate (exclusive lock), verify cached",
        "insta" => {
            "accept/reject mutate (exclusive lock); review runs directly in the pool, test as exec"
        }
        "machete" | "shear" => "verify cached; --fix mutate (exclusive lock)",
        "udeps" | "public-api" => "verify cached (toolchain is part of the key)",
        "hack" => "check/clippy verify cached; other commands exec",
        "sqlx" => "prepare mutate (exclusive lock); others exec",
        "sweep" | "clean-all" => "mutate (exclusive lock); bypasses heli's build store",
        "dx" | "trunk" => "serve runs directly in the pool, never attached to; others exec",
        "audit" => "verify cached, keyed by the advisory database heli fetches first; fix mutate",
        "deny" => "check verify cached, keyed by the advisory database heli fetches first",
        _ => return None,
    })
}

/// The subcommand of `cargo <args>`, first or after a `+toolchain` (as
/// rustup's cargo takes it), and the arguments after it.
pub fn subcommand(args: &[OsString]) -> Option<(&OsString, &[OsString])> {
    let args = match args.first() {
        Some(first) if first.to_string_lossy().starts_with('+') => &args[1..],
        _ => args,
    };
    args.split_first()
}

/// For a test run `cargo <args>`: the tool that runs its tests, and how
/// many of `args` name the toolchain and the subcommand (`test`, or
/// `nextest run`), before the tool's own arguments. `None` for any other
/// command.
pub fn test_runner(args: &[OsString]) -> Option<(Runner, usize)> {
    let (subcommand, rest) = subcommand(args)?;
    let at = args.len() - rest.len();
    match subcommand.to_str()? {
        "test" | "t" => Some((Runner::Libtest, at)),
        "nextest" if rest.first().is_some_and(|run| run == "run" || run == "r") => {
            Some((Runner::Nextest, at + 1))
        }
        _ => None,
    }
}

impl Class {
    /// The class of `cargo <args>`, `args` starting with the subcommand.
    pub fn of_cargo(args: &[OsString]) -> Self {
        if test_runner(args).is_some() {
            return Self::Test;
        }
        let Some((subcommand, rest)) = subcommand(args) else {
            return Self::Exec;
        };
        let before_separator = || rest.iter().take_while(|arg| *arg != "--");
        match subcommand.to_str().unwrap_or_default() {
            "build" | "b" | "run" | "r" | "doc" | "d" => Self::Compile,
            "check" | "c" => Self::Verify,
            "clippy" if before_separator().any(|arg| arg == "--fix") => Self::Mutate,
            "clippy" => Self::Verify,
            "fmt" if rest.iter().any(|arg| arg == "--check") => Self::Verify,
            "fmt" | "fix" => Self::Mutate,
            tool => third_party(tool, rest),
        }
    }

    /// The name `status` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::Verify => "verify",
            Self::Test => "test",
            Self::Mutate => "mutate",
            Self::Exec => "exec",
        }
    }

    /// How a run of this class holds its worktree's lock: exclusive for the
    /// runs that rewrite sources, shared for the others.
    pub fn worktree_lock(self) -> Access {
        match self {
            Self::Mutate => Access::Exclusive,
            Self::Compile | Self::Verify | Self::Test | Self::Exec => Access::Shared,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn interactive_tools_run_directly_with_or_without_a_terminal() {
        for words in [
            &["watch", "-x", "check"][..],
            &["leptos", "serve"],
            &["tauri", "dev"],
            &["insta", "review"][..],
            &["dx", "serve"],
            &["+nightly", "trunk", "serve"],
        ] {
            assert!(runs_directly(&args(words), false), "{words:?}");
        }
        assert!(!runs_directly(&args(&["insta", "accept"]), true));
    }

    #[test]
    fn unknown_tools_run_directly_only_at_a_terminal() {
        let zig = args(&["zigbuild", "--release"]);
        assert!(runs_directly(&zig, true));
        assert!(!runs_directly(&zig, false));
    }

    #[test]
    fn known_commands_stay_in_the_daemon_at_a_terminal() {
        for words in [
            &["build"][..],
            &["bench"],
            &["tree"],
            &["audit"],
            &["hack", "test"],
            &["nextest", "list"],
            &["--version"],
            &[],
        ] {
            assert!(!runs_directly(&args(words), true), "{words:?}");
        }
    }
}
