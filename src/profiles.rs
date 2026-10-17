//! Tool profiles from configuration (`DESIGN.md` §5, "Tool profiles"): the
//! user's, in heli's configuration file, and a repository's, in its
//! committed `.config/heli.toml`. Both use the same table:
//!
//! ```toml
//! [[profiles.tool]]
//! tool = "xtask"              # cargo-<tool>, or the alias, after `cargo`
//! args = ["check-changed"]    # optional: the arguments it starts with
//! class = "verify"            # compile, verify, mutate or exec
//! ```
//!
//! **Resolution.** Built-in profiles (`crate::class`), then the user's, then
//! the repository's; within one source the rule with the longest `args`
//! wins, and between rules of the same length the last. `test` is not a
//! class a profile can give: a test run needs a runner heli knows.
//!
//! **A repository only narrows.** Its profile applies only when it is at
//! least as strict as what the user's and the built-in ones give: it may
//! take the worktree lock exclusively, or stop caching, never cache or share
//! more. With `trust_repository = true` under `[profiles]` in the user's
//! configuration, the repository's profiles apply as they are.
//!
//! An unreadable or invalid repository file is reported on stderr and
//! ignored: the built-in and user profiles still apply. The user's file is
//! read with the rest of the client's configuration, where errors are
//! terminal.

use std::ffi::OsString;
use std::path::Path;

use serde::Deserialize;

use crate::class::{self, Access, Class};

/// The repository's profile file, relative to its worktree.
pub const REPOSITORY_FILE: &str = ".config/heli.toml";

/// The `[profiles]` table.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Profiles {
    /// Let the repository's profiles loosen what the user's give.
    pub trust_repository: bool,
    pub tool: Vec<Rule>,
}

/// One profile.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub tool: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub class: RuleClass,
}

/// The classes a profile can give.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleClass {
    Compile,
    Verify,
    Mutate,
    Exec,
}

impl From<RuleClass> for Class {
    fn from(class: RuleClass) -> Self {
        match class {
            RuleClass::Compile => Self::Compile,
            RuleClass::Verify => Self::Verify,
            RuleClass::Mutate => Self::Mutate,
            RuleClass::Exec => Self::Exec,
        }
    }
}

/// Where a run's class came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Cargo's own subcommands, or no profile at all.
    BuiltIn,
    User,
    Repository,
}

impl Source {
    /// How `status` and the event log name where the class of `cargo
    /// <args>` came from: `user` or `repository` for their profiles; for the
    /// built-in ones `cargo` (cargo's own subcommands), `built-in` (a tool
    /// heli has a profile for) or `unknown` (any other tool, which a profile
    /// should be written for).
    pub fn name(self, args: &[OsString]) -> &'static str {
        match self {
            Self::User => "user",
            Self::Repository => "repository",
            Self::BuiltIn => {
                let tool = class::subcommand(args)
                    .and_then(|(tool, _)| tool.to_str())
                    .unwrap_or_default();
                if class::tool_profile(tool).is_some() {
                    "built-in"
                } else if tool.is_empty()
                    || tool.starts_with('-')
                    || Class::of_cargo(args) != Class::Exec
                    || class::is_cargo_builtin(tool)
                {
                    "cargo"
                } else {
                    "unknown"
                }
            }
        }
    }
}

/// The repository file's `[profiles]` table.
#[derive(Debug, Default, Deserialize)]
struct RepositoryFile {
    #[serde(default)]
    profiles: RepositoryProfiles,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RepositoryProfiles {
    tool: Vec<Rule>,
}

/// The repository's profiles, from `worktree`; `Err` says why the file
/// cannot be used. No file is no profiles.
pub fn repository(worktree: &Path) -> Result<Vec<Rule>, String> {
    let path = worktree.join(REPOSITORY_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    toml::from_str::<RepositoryFile>(&text)
        .map(|file| file.profiles.tool)
        .map_err(|error| format!("invalid {}: {error}", path.display()))
}

/// The rule of `rules` that matches `tool rest…`: the longest `args`
/// prefix, the last among equals.
fn matching<'a>(rules: &'a [Rule], tool: &str, rest: &[OsString]) -> Option<&'a Rule> {
    rules
        .iter()
        .filter(|rule| {
            rule.tool == tool
                && rule.args.len() <= rest.len()
                && rule
                    .args
                    .iter()
                    .zip(rest)
                    .all(|(want, arg)| arg == want.as_str())
        })
        .max_by_key(|rule| rule.args.len())
}

/// How much a class caches or shares: higher is stricter.
fn cache_strictness(class: Class) -> u8 {
    match class {
        Class::Verify | Class::Test => 0,
        Class::Compile => 1,
        Class::Exec | Class::Mutate => 2,
    }
}

/// Whether `narrower` is at least as strict as `base` in caching and in the
/// worktree lock.
fn at_least_as_strict(narrower: Class, base: Class) -> bool {
    let exclusive = |class: Class| class.worktree_lock() == Access::Exclusive;
    cache_strictness(narrower) >= cache_strictness(base)
        && (exclusive(narrower) || !exclusive(base))
}

/// The class of `cargo <args>`, and where it came from, with `user` and
/// `repository` profiles over the built-in ones.
pub fn resolve(args: &[OsString], user: &Profiles, repository: &[Rule]) -> (Class, Source) {
    let builtin = Class::of_cargo(args);
    let Some((tool, rest)) = class::subcommand(args) else {
        return (builtin, Source::BuiltIn);
    };
    let Some(tool) = tool.to_str() else {
        return (builtin, Source::BuiltIn);
    };
    let (base, source) = match matching(&user.tool, tool, rest) {
        Some(rule) => (rule.class.into(), Source::User),
        None => (builtin, Source::BuiltIn),
    };
    match matching(repository, tool, rest) {
        Some(rule) if user.trust_repository || at_least_as_strict(rule.class.into(), base) => {
            (rule.class.into(), Source::Repository)
        }
        _ => (base, source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn rule(tool: &str, words: &[&str], class: RuleClass) -> Rule {
        Rule {
            tool: tool.to_owned(),
            args: words.iter().map(|word| (*word).to_owned()).collect(),
            class,
        }
    }

    #[test]
    fn a_user_profile_classifies_a_tool() {
        let user = Profiles {
            tool: vec![rule("xtask", &["check-changed"], RuleClass::Verify)],
            ..Profiles::default()
        };
        assert_eq!(
            resolve(&args(&["xtask", "check-changed", "-v"]), &user, &[]),
            (Class::Verify, Source::User)
        );
        assert_eq!(
            resolve(&args(&["xtask", "dist"]), &user, &[]),
            (Class::Exec, Source::BuiltIn)
        );
    }

    #[test]
    fn the_longest_prefix_wins() {
        let user = Profiles {
            tool: vec![
                rule("xtask", &[], RuleClass::Mutate),
                rule("xtask", &["lint"], RuleClass::Verify),
            ],
            ..Profiles::default()
        };
        assert_eq!(
            resolve(&args(&["xtask", "lint"]), &user, &[]).0,
            Class::Verify
        );
        assert_eq!(
            resolve(&args(&["xtask", "gen"]), &user, &[]).0,
            Class::Mutate
        );
    }

    #[test]
    fn a_repository_may_only_narrow() {
        let user = Profiles::default();
        let loosen = [rule("xtask", &["check"], RuleClass::Verify)];
        assert_eq!(
            resolve(&args(&["xtask", "check"]), &user, &loosen),
            (Class::Exec, Source::BuiltIn)
        );
        let narrow = [rule("machete", &[], RuleClass::Exec)];
        assert_eq!(
            resolve(&args(&["machete"]), &user, &narrow),
            (Class::Exec, Source::Repository)
        );
        let lock = [rule("xtask", &["gen"], RuleClass::Mutate)];
        assert_eq!(
            resolve(&args(&["xtask", "gen"]), &user, &lock),
            (Class::Mutate, Source::Repository)
        );
        // Mutate to exec would give up the exclusive lock.
        let unlock = [rule("add", &[], RuleClass::Exec)];
        assert_eq!(
            resolve(&args(&["add", "serde"]), &user, &unlock).0,
            Class::Mutate
        );
    }

    #[test]
    fn a_trusted_repository_may_loosen() {
        let user = Profiles {
            trust_repository: true,
            ..Profiles::default()
        };
        let loosen = [rule("xtask", &["check"], RuleClass::Verify)];
        assert_eq!(
            resolve(&args(&["xtask", "check"]), &user, &loosen),
            (Class::Verify, Source::Repository)
        );
    }

    #[test]
    fn the_repository_file_is_read_from_the_worktree() {
        let dir = tempfile::tempdir().expect("dir");
        assert_eq!(repository(dir.path()), Ok(Vec::new()));
        std::fs::create_dir_all(dir.path().join(".config")).expect("dir");
        std::fs::write(
            dir.path().join(REPOSITORY_FILE),
            "[[profiles.tool]]\ntool = \"xtask\"\nclass = \"mutate\"\n",
        )
        .expect("write");
        assert_eq!(
            repository(dir.path()),
            Ok(vec![rule("xtask", &[], RuleClass::Mutate)])
        );
        std::fs::write(
            dir.path().join(REPOSITORY_FILE),
            "[[profiles.tool]]\nclass = \"test\"\n",
        )
        .expect("write");
        assert!(repository(dir.path()).is_err());
    }
}
