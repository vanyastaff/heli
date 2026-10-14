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

impl Class {
    /// The class of `cargo <args>`, `args` starting with the subcommand.
    pub fn of_cargo(args: &[OsString]) -> Self {
        let args = match args.first() {
            Some(first) if first.to_string_lossy().starts_with('+') => &args[1..],
            _ => args,
        };
        let Some((subcommand, rest)) = args.split_first() else {
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
            "test" | "t" => Self::Test,
            "nextest" if rest.first().is_some_and(|run| run == "run" || run == "r") => Self::Test,
            _ => Self::Exec,
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
