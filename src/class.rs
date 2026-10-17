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

/// A built-in tool profile (`DESIGN.md` §5, "Tool profiles"): what heli
/// does with `cargo <name> <args>` for each of `names`, as a declarative
/// record. The first of `rules` whose condition holds decides, else
/// `otherwise`. Tools that rewrite the worktree are mutate runs, so that they
/// never run under a build; tools that only read it and report are verify
/// runs, shared when nothing changed; interactive and long-running ones run
/// directly in the pool. A tool not in [`TOOLS`] is exec.
struct Tool {
    names: &'static [&'static str],
    /// What the profile does, as `cargo heli doctor` says it.
    summary: &'static str,
    /// The tool's options that take a value: skipped, with it, when looking
    /// for the first word.
    valued: &'static [&'static str],
    rules: &'static [(When, Then)],
    otherwise: Then,
    /// For a tool that deletes build output itself, what heli says before
    /// it runs: such a run holds heli's eviction lock throughout, so that
    /// heli's own eviction and the tool never delete at the same time.
    cleaner: Option<&'static str>,
}

/// A condition on a tool's arguments, all before any `--`.
#[derive(Clone, Copy)]
enum When {
    /// The first word (not an option, nor an option's value) is one of these.
    Word(&'static [&'static str]),
    /// There is a first word at all.
    AnyWord,
    /// One of these arguments is there, anywhere.
    Arg(&'static [&'static str]),
}

/// What a profile makes of a run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Then {
    /// The daemon runs it, as a run of this class.
    Run(Class),
    /// The client runs it itself, drawing from the host pool: interactive
    /// and long-running tools, which need a terminal and hold a slot for
    /// hours. Its class is exec.
    Direct,
}

use Then::{Direct, Run};

/// Every built-in profile. Each name is in one profile only (a test checks).
const TOOLS: &[Tool] = &[
    Tool {
        // Cargo's own manifest editors, cargo-edit's, cargo-release (which
        // also bumps versions and commits) and cargo-autoinherit.
        names: &[
            "add",
            "remove",
            "rm",
            "update",
            "upgrade",
            "set-version",
            "release",
            "autoinherit",
        ],
        summary: "mutate: exclusive worktree lock",
        valued: &[],
        rules: &[],
        otherwise: Run(Class::Mutate),
        cleaner: None,
    },
    Tool {
        names: &["sort"],
        summary: "--check verify cached; else mutate (exclusive lock)",
        valued: &[],
        rules: &[(When::Arg(&["--check", "-c"]), Run(Class::Verify))],
        otherwise: Run(Class::Mutate),
        cleaner: None,
    },
    Tool {
        names: &["machete", "shear"],
        summary: "verify cached; --fix mutate (exclusive lock)",
        valued: &[],
        rules: &[(When::Arg(&["--fix"]), Run(Class::Mutate))],
        otherwise: Run(Class::Verify),
        cleaner: None,
    },
    Tool {
        // Miri interprets the tests: a verdict on the tree and the nightly
        // toolchain, which the `+toolchain` puts in the key. `run` runs a
        // program, `setup` builds Miri's standard library.
        names: &["miri"],
        summary: "test/nextest verify cached (toolchain is part of the key); others exec",
        valued: &[],
        rules: &[(When::Word(&["test", "nextest"]), Run(Class::Verify))],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["udeps", "public-api"],
        summary: "verify cached (toolchain is part of the key)",
        valued: &[],
        rules: &[],
        otherwise: Run(Class::Verify),
        cleaner: None,
    },
    Tool {
        names: &["hakari"],
        summary: "generate/manage-deps mutate (exclusive lock), verify cached",
        valued: &[],
        rules: &[
            (When::Word(&["generate", "manage-deps"]), Run(Class::Mutate)),
            (When::Word(&["verify"]), Run(Class::Verify)),
        ],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["insta"],
        summary: "accept/reject mutate (exclusive lock); review runs directly in the pool, test as exec",
        valued: &[],
        rules: &[
            (When::Word(&["accept", "reject"]), Run(Class::Mutate)),
            (When::Word(&["review"]), Direct),
        ],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["sqlx"],
        summary: "prepare mutate (exclusive lock); others exec",
        valued: &[],
        rules: &[(When::Word(&["prepare"]), Run(Class::Mutate))],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["sweep"],
        summary: "mutate (exclusive lock), never alongside heli's eviction; sees only target/",
        valued: &[],
        rules: &[],
        otherwise: Run(Class::Mutate),
        cleaner: Some(
            "cargo-sweep sees only target/: heli keeps the intermediates in its build store, \
             which `cargo heli gc` evicts by age and budget. heli's own eviction waits until \
             this run ends.",
        ),
    },
    Tool {
        names: &["clean-all"],
        summary: "mutate (exclusive lock), never alongside heli's eviction; deletes other worktrees' target/",
        valued: &[],
        rules: &[],
        otherwise: Run(Class::Mutate),
        cleaner: Some(
            "cargo-clean-all deletes whole target/ directories, those of other worktrees heli \
             may be building in right now included: heli holds only this worktree's lock. \
             `cargo heli gc` evicts only what no build uses. heli's own eviction waits until \
             this run ends.",
        ),
    },
    Tool {
        // A verdict on the tree and the advisory database, whose commit is
        // in the key; the daemon fetches the database first for a run that
        // would (`crate::daemon::advisory`). `fix` rewrites manifests; `bin`
        // scans binaries outside the tree.
        names: &["audit"],
        summary: "verify cached, keyed by the advisory database heli fetches first; fix mutate",
        valued: &[
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
        ],
        rules: &[
            (When::Word(&["fix"]), Run(Class::Mutate)),
            (When::AnyWord, Run(Class::Exec)),
        ],
        otherwise: Run(Class::Verify),
        cleaner: None,
    },
    Tool {
        names: &["deny"],
        summary: "check verify cached, keyed by the advisory database heli fetches first",
        valued: &[],
        rules: &[(When::Arg(&["check"]), Run(Class::Verify))],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        // It runs check or clippy over feature combinations.
        names: &["hack"],
        summary: "check/clippy verify cached; other commands exec",
        valued: &[],
        rules: &[(When::Arg(&["check", "clippy"]), Run(Class::Verify))],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["dx", "trunk"],
        summary: "serve runs directly in the pool, never attached to; others exec",
        valued: &[],
        rules: &[(When::Word(&["serve"]), Direct)],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["leptos"],
        summary: "watch/serve run directly in the pool; others exec",
        valued: &[],
        rules: &[(When::Word(&["watch", "serve"]), Direct)],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["tauri"],
        summary: "dev runs directly in the pool; others exec",
        valued: &[],
        rules: &[(When::Word(&["dev"]), Direct)],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        names: &["watch"],
        summary: "runs directly in the pool, never attached to",
        valued: &[],
        rules: &[],
        otherwise: Direct,
        cleaner: None,
    },
    Tool {
        names: &["outdated", "semver-checks", "vet", "geiger"],
        summary: "exec: never cached, the result depends on the registry",
        valued: &[],
        rules: &[],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        // Its `target/llvm-cov-target` is a built-in tool cache
        // (`crate::config::BUILTIN_TOOL_CACHES`).
        names: &["llvm-cov"],
        summary: "exec; its target/llvm-cov-target is a tool cache heli accounts for and evicts",
        valued: &[],
        rules: &[],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
    Tool {
        // Its scratch copies of the workspace are eviction tier 0
        // (`crate::store::scratch`).
        names: &["mutants"],
        summary: "exec; its scratch copies of the workspace are evicted first (tier 0)",
        valued: &[],
        rules: &[],
        otherwise: Run(Class::Exec),
        cleaner: None,
    },
];

impl Tool {
    /// The built-in profile of `cargo <name>`, if heli has one.
    fn named(name: &str) -> Option<&'static Tool> {
        TOOLS.iter().find(|tool| tool.names.contains(&name))
    }

    /// What this profile makes of `cargo <name> <rest>`.
    fn decide(&self, rest: &[OsString]) -> Then {
        let args: Vec<&OsString> = rest.iter().take_while(|arg| *arg != "--").collect();
        let word = self.first_word(&args);
        let holds = |when: &When| match *when {
            When::Word(words) => word.is_some_and(|word| words.contains(&word)),
            When::AnyWord => word.is_some(),
            When::Arg(wanted) => args
                .iter()
                .any(|arg| wanted.iter().any(|want| *arg == want)),
        };
        self.rules
            .iter()
            .find(|(when, _)| holds(when))
            .map_or(self.otherwise, |(_, then)| *then)
    }

    /// The first of `args` that is neither an option nor the value of one
    /// of [`Tool::valued`]. One that is not Unicode counts as a word no rule
    /// names.
    fn first_word<'a>(&self, args: &[&'a OsString]) -> Option<&'a str> {
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let Some(arg) = arg.to_str() else {
                return Some("");
            };
            if self.valued.contains(&arg) {
                args.next();
            } else if !arg.starts_with('-') {
                return Some(arg);
            }
        }
        None
    }
}

/// The class of `cargo <tool> <rest>` for a subcommand cargo does not run
/// itself: its built-in profile's, else exec.
fn third_party(tool: &str, rest: &[OsString]) -> Class {
    match Tool::named(tool).map(|profile| profile.decide(rest)) {
        Some(Run(class)) => class,
        Some(Direct) | None => Class::Exec,
    }
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
/// (`DESIGN.md` §5, "Tool profiles"), instead of in the daemon: what a
/// built-in profile runs directly ([`TOOLS`]), and, when stdin is a
/// terminal, every exec command of a tool heli has no profile for.
pub fn runs_directly(args: &[OsString], stdin_is_terminal: bool) -> bool {
    let Some((tool, rest)) = subcommand(args) else {
        return false;
    };
    let tool = tool.to_str().unwrap_or_default();
    match Tool::named(tool) {
        Some(profile) => profile.decide(rest) == Direct,
        None => {
            stdin_is_terminal
                && !tool.is_empty()
                && !tool.starts_with('-')
                && Class::of_cargo(args) == Class::Exec
                && !is_cargo_builtin(tool)
        }
    }
}

/// For `cargo <args>` whose tool deletes build output itself (cargo-sweep,
/// cargo-clean-all), what heli says before it runs; such a run holds heli's
/// eviction lock throughout (`crate::client`). `None` for any other.
pub fn cleaner_note(args: &[OsString]) -> Option<&'static str> {
    let (tool, _) = subcommand(args)?;
    Tool::named(tool.to_str()?)?.cleaner
}

/// What heli's built-in profile does with the cargo tool `cargo-<name>`, for
/// `cargo heli doctor`; `None` for a tool heli has no profile for (it runs
/// as exec).
pub fn tool_profile(name: &str) -> Option<&'static str> {
    Tool::named(name).map(|profile| profile.summary)
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

    /// Each name is in one profile only, and no profile shadows cargo's own
    /// subcommands, so lookup order never matters.
    #[test]
    fn every_tool_name_has_one_profile() {
        let mut names: Vec<&str> = TOOLS
            .iter()
            .flat_map(|tool| tool.names.iter().copied())
            .collect();
        let all = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all, "a name in two profiles");
        for name in names {
            assert!(!is_cargo_builtin(name), "{name} shadows cargo's own");
            assert!(!tool_profile(name).unwrap_or_default().is_empty(), "{name}");
        }
    }

    /// Options with values do not count as words; arguments after `--` do
    /// not count at all.
    #[test]
    fn values_and_what_follows_the_separator_are_not_words() {
        assert_eq!(
            Class::of_cargo(&args(&["audit", "--db", "x"])),
            Class::Verify
        );
        assert_eq!(
            Class::of_cargo(&args(&["audit", "bin", "app"])),
            Class::Exec
        );
        assert_eq!(
            Class::of_cargo(&args(&["machete", "--", "--fix"])),
            Class::Verify
        );
        assert_eq!(
            Class::of_cargo(&args(&["hakari", "-q", "generate"])),
            Class::Mutate
        );
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
