//! `--changed` and `--affected` (docs/runs.md): run a cargo subcommand on
//! the workspace members that changed since a base, and with `--affected`
//! on every member that depends on them too; `cargo heli changed` only
//! prints that selection and why.
//!
//! - **Base:** the merge base of `HEAD` with a reference: the one given,
//!   else the branch's upstream (`@{upstream}`), else `origin/HEAD`.
//! - **Files:** what `git diff <base>` names (commits since the base,
//!   staged and unstaged changes) and the untracked files git does not
//!   ignore.
//! - **File → package:** the deepest member whose manifest directory holds
//!   the file. `Cargo.lock` and the root `Cargo.toml`, `.cargo/` and
//!   `rust-toolchain*` (in the workspace root or a directory above it in the
//!   repository) select the whole workspace: `--workspace`. Any other file
//!   selects nothing.
//! - **Dependents:** the members that depend on a selected one, through
//!   normal, dev or build dependencies, transitively, as `cargo metadata
//!   --no-deps` declares them.
//!
//! The selection reaches cargo as `-p a -p b`, sorted, so that the same
//! selection gives the same run key (`crate::daemon::key`).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::store::probe::Probe;

/// Exit code for a selection heli refuses or cannot make.
const REFUSED: i32 = 2;

/// The version of `cargo heli changed --json`.
const SCHEMA: u32 = 1;

/// What `--changed` or `--affected` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// `--affected`: the dependents too.
    pub affected: bool,
    /// The reference given (`=REF`), else the default.
    pub base: Option<String>,
}

impl Selection {
    /// The flag as the user gave it, for messages.
    fn flag(&self) -> &'static str {
        if self.affected {
            "--affected"
        } else {
            "--changed"
        }
    }
}

/// The cargo subcommands that take `-p`, which a selection can be given to.
const SUPPORTED: &str =
    "build, check, clippy, test, bench, doc, nextest run, nextest list and llvm-cov";

/// Why heli refuses `flag` (`--changed` or `--affected`) for `cargo <args>`:
/// a subcommand that takes no `-p`, or arguments that already choose the
/// packages.
pub fn refusal(flag: &str, args: &[OsString]) -> Option<String> {
    let Some((subcommand, rest)) = crate::class::subcommand(args) else {
        return Some(format!("{flag} needs a cargo subcommand"));
    };
    let subcommand = subcommand.to_string_lossy();
    let first = rest.first().map(|arg| arg.to_string_lossy());
    let takes_packages = match &*subcommand {
        "build" | "b" | "check" | "c" | "clippy" | "test" | "t" | "bench" | "doc" | "d" => true,
        "nextest" => matches!(first.as_deref(), Some("run" | "r" | "list")),
        "llvm-cov" => !matches!(first.as_deref(), Some("report" | "clean" | "show-env")),
        _ => false,
    };
    if !takes_packages {
        let command = match first {
            Some(first) if subcommand == "nextest" || subcommand == "llvm-cov" => {
                format!("{subcommand} {first}")
            }
            _ => subcommand.into_owned(),
        };
        return Some(format!(
            "{flag} works with {SUPPORTED}, which take -p; not with `cargo {command}`"
        ));
    }
    let chooser = rest
        .iter()
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| arg.to_str())
        .find(|arg| chooses_packages(arg))?;
    Some(format!(
        "{flag} chooses the packages itself; it cannot be combined with `{chooser}`"
    ))
}

/// Whether the cargo argument `arg` chooses packages: `-p`, `--package`,
/// `--workspace`, `--all`, `--exclude`, or nextest's `-E`.
fn chooses_packages(arg: &str) -> bool {
    let name = arg.split_once('=').map_or(arg, |(name, _)| name);
    matches!(
        name,
        "--package" | "--workspace" | "--all" | "--exclude" | "--filterset" | "--filter-expr"
    ) || arg.starts_with("-p")
        || arg.starts_with("-E")
}

/// The commit a selection compares with.
#[derive(Debug, Clone)]
pub struct Base {
    /// The reference, as given or as git abbreviates the default one.
    pub reference: String,
    /// The merge base of `HEAD` with it.
    pub commit: String,
    /// Its abbreviated hash.
    pub short: String,
}

/// Why a package is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Its files changed: these, as git names them.
    Changed(Vec<String>),
    /// It depends on this selected member.
    Depends(String),
}

/// A selected member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub name: String,
    pub why: Why,
}

/// What a selection chose, and why.
#[derive(Debug)]
pub struct Choice {
    pub base: Base,
    /// The files that select the whole workspace, if any did.
    pub workspace: Vec<String>,
    /// The selected members, in the order they were found: changed first.
    pub packages: Vec<Chosen>,
    /// The changed files that select nothing.
    pub outside: Vec<String>,
}

impl Choice {
    /// Whether nothing was selected.
    pub fn is_empty(&self) -> bool {
        self.workspace.is_empty() && self.packages.is_empty()
    }

    /// The cargo arguments: `--workspace`, else `-p` for each package,
    /// sorted.
    pub fn args(&self) -> Vec<OsString> {
        if !self.workspace.is_empty() {
            return vec![OsString::from("--workspace")];
        }
        let names: BTreeSet<&str> = self.packages.iter().map(|chosen| &*chosen.name).collect();
        names
            .into_iter()
            .flat_map(|name| [OsString::from("-p"), OsString::from(name)])
            .collect()
    }

    /// `<base> (<short>)`.
    fn since(&self) -> String {
        format!("{} ({})", self.base.reference, self.base.short)
    }

    /// The line heli prints before running with this selection, for `flag`.
    fn line(&self, flag: &str) -> String {
        let args = self
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let dependents = self
            .packages
            .iter()
            .filter(|chosen| matches!(chosen.why, Why::Depends(_)))
            .count();
        let detail = if !self.workspace.is_empty() {
            format!(" ({} changed)", self.workspace.join(", "))
        } else if dependents > 0 {
            let plural = if dependents == 1 { "" } else { "s" };
            format!(" (+{dependents} dependent{plural})")
        } else {
            String::new()
        };
        format!("cargo-heli: {flag} since {}: {args}{detail}", self.since())
    }
}

/// git with arguments in a directory: its stdout, or why it failed.
type Git<'a> = dyn Fn(&[&str], &Path) -> Result<String, String> + 'a;

/// A workspace member, as the selection sees it.
#[derive(Debug)]
struct Member {
    name: String,
    dir: PathBuf,
    /// The members it depends on, by index.
    depends_on: Vec<usize>,
}

/// Chooses what `selection` selects for `cargo <args>` run with `cargo` in
/// `cwd`.
pub fn choose(
    selection: &Selection,
    cwd: &Path,
    cargo: OsString,
    args: &[OsString],
    base_flag: &str,
) -> Result<Choice, String> {
    let probe = Probe::cargo_in(cwd.to_path_buf(), cargo, args);
    let workspace = probe.members()?;
    let root = crate::store::simplified(&workspace.root);
    let mut members: Vec<Member> = workspace
        .packages
        .iter()
        .map(|package| Member {
            name: package.name.clone(),
            dir: crate::store::simplified(package.manifest_path.parent().unwrap_or(Path::new(""))),
            depends_on: Vec::new(),
        })
        .collect();
    for (index, package) in workspace.packages.iter().enumerate() {
        let depends_on = package
            .dependencies
            .iter()
            .filter_map(|dependency| dependency.path.as_deref())
            .map(crate::store::simplified)
            .filter_map(|path| members.iter().position(|member| member.dir == path))
            .collect();
        members[index].depends_on = depends_on;
    }
    let git = |args: &[&str], dir: &Path| -> Result<String, String> {
        probe
            .git_in(dir, args, None)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let top = git(&["rev-parse", "--show-toplevel"], &root).map_err(|why| {
        format!(
            "cannot find the git repository of {}: {why}",
            root.display()
        )
    })?;
    let top = crate::store::simplified(Path::new(top.trim_end()));
    let base = base(&git, &top, selection.base.as_deref(), base_flag)?;
    let mut files = Vec::new();
    for listing in [
        git(
            &[
                "diff",
                "--name-only",
                "--no-renames",
                "--no-relative",
                "-z",
                &base.commit,
            ],
            &top,
        )?,
        git(&["ls-files", "--others", "--exclude-standard", "-z"], &top)?,
    ] {
        files.extend(
            listing
                .split('\0')
                .filter(|file| !file.is_empty())
                .map(str::to_owned),
        );
    }
    files.sort();
    files.dedup();
    let mut choice = attribute(&members, &root, &top, &files, base);
    if selection.affected {
        add_dependents(&members, &mut choice.packages);
    }
    Ok(choice)
}

/// The base `given` names (else `@{upstream}`, else `origin/HEAD`), with
/// `git` run in `top`. Without either default, the error says to give one
/// with `flag`.
fn base(git: &Git, top: &Path, given: Option<&str>, flag: &str) -> Result<Base, String> {
    let reference = match given {
        Some(given) => given.to_owned(),
        None => ["@{upstream}", "origin/HEAD"]
            .into_iter()
            .find(|candidate| git(&["rev-parse", "--verify", "--quiet", candidate], top).is_ok())
            .map(|candidate| {
                git(&["rev-parse", "--abbrev-ref", candidate], top)
                    .map(|name| name.trim().to_owned())
                    .ok()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| candidate.to_owned())
            })
            .ok_or_else(|| {
                format!(
                    "no base to compare with: the branch has no upstream and there is no \
                     origin/HEAD; give one: {flag}"
                )
            })?,
    };
    let commit = git(&["merge-base", "HEAD", &reference], top)
        .map_err(|why| format!("cannot find the merge base of HEAD and {reference}: {why}"))?
        .trim()
        .to_owned();
    let short = git(&["rev-parse", "--short", &commit], top)
        .map(|short| short.trim().to_owned())
        .unwrap_or_else(|_| commit.chars().take(7).collect());
    Ok(Base {
        reference,
        commit,
        short,
    })
}

/// Which members `files` (relative to the repository's top `top`) select
/// in the workspace at `root`.
fn attribute(members: &[Member], root: &Path, top: &Path, files: &[String], base: Base) -> Choice {
    let mut choice = Choice {
        base,
        workspace: Vec::new(),
        packages: Vec::new(),
        outside: Vec::new(),
    };
    for file in files {
        let path = top.join(file);
        if whole_workspace(&path, root, top) {
            choice.workspace.push(file.clone());
            continue;
        }
        let owner = members
            .iter()
            .filter(|member| path.starts_with(&member.dir))
            .max_by_key(|member| member.dir.components().count());
        let Some(owner) = owner else {
            choice.outside.push(file.clone());
            continue;
        };
        let existing = choice
            .packages
            .iter_mut()
            .find(|chosen| chosen.name == owner.name);
        match existing {
            Some(Chosen {
                why: Why::Changed(files),
                ..
            }) => files.push(file.clone()),
            _ => choice.packages.push(Chosen {
                name: owner.name.clone(),
                why: Why::Changed(vec![file.clone()]),
            }),
        }
    }
    choice
}

/// Whether the file at `path` changes the whole workspace at `root`:
/// `Cargo.lock` or the root `Cargo.toml`, or `.cargo/` or `rust-toolchain*`
/// in the root or a directory above it, up to the repository's top `top`.
fn whole_workspace(path: &Path, root: &Path, top: &Path) -> bool {
    if path == root.join("Cargo.lock") || path == root.join("Cargo.toml") {
        return true;
    }
    let mut dirs = vec![root];
    dirs.extend(
        root.ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(top)),
    );
    dirs.into_iter().any(|dir| {
        path.starts_with(dir.join(".cargo"))
            || (path.parent() == Some(dir)
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("rust-toolchain")))
    })
}

/// Adds to `chosen` every member that depends on one of them, transitively.
fn add_dependents(members: &[Member], chosen: &mut Vec<Chosen>) {
    let mut next = 0;
    while next < chosen.len() {
        let name = chosen[next].name.clone();
        next += 1;
        let Some(index) = members.iter().position(|member| member.name == name) else {
            continue;
        };
        for member in members
            .iter()
            .filter(|member| member.depends_on.contains(&index))
        {
            if !chosen.iter().any(|chosen| chosen.name == member.name) {
                chosen.push(Chosen {
                    name: member.name.clone(),
                    why: Why::Depends(name.clone()),
                });
            }
        }
    }
}

/// Gives `cargo <args>`, run with `cargo`, the selection `selection` asks
/// for: the `-p` list goes before the first `--`. Says what it chose on
/// stderr unless `quiet`. Exits 0 when nothing changed, and 2 when the
/// selection cannot be made.
pub fn apply(selection: &Selection, cargo: &OsString, args: &mut Vec<OsString>, quiet: bool) {
    let flag = selection.flag();
    let choice = std::env::current_dir()
        .map_err(|error| format!("cannot read the working directory: {error}"))
        .and_then(|cwd| choose(selection, &cwd, cargo.clone(), args, &format!("{flag}=REF")))
        .unwrap_or_else(|why| {
            eprintln!("cargo-heli: {flag}: {why}");
            std::process::exit(REFUSED);
        });
    if choice.is_empty() {
        eprintln!("cargo-heli: nothing changed since {}", choice.since());
        std::process::exit(0);
    }
    if !quiet {
        eprintln!("{}", choice.line(flag));
    }
    let at = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.splice(at..at, choice.args());
}

/// `cargo heli changed`: prints what `--changed` (or with `affected`,
/// `--affected`) would select since `base`, and why; as JSON with `json`.
pub fn main(affected: bool, base: Option<String>, json: bool) -> ! {
    let selection = Selection { affected, base };
    let cargo = std::env::var_os("CARGO")
        .filter(|cargo| !cargo.is_empty())
        .unwrap_or_else(|| OsString::from("cargo"));
    let choice = std::env::current_dir()
        .map_err(|error| format!("cannot read the working directory: {error}"))
        .and_then(|cwd| choose(&selection, &cwd, cargo, &[], "--base REF"))
        .unwrap_or_else(|why| {
            eprintln!("cargo-heli: {why}");
            std::process::exit(REFUSED);
        });
    if json {
        println!("{}", document(&choice, affected));
    } else {
        print!("{}", text(&choice));
    }
    std::process::exit(0)
}

/// `cargo heli changed`'s text: the base, each selected package and why,
/// the files that select nothing, and the cargo arguments.
fn text(choice: &Choice) -> String {
    if choice.is_empty() && choice.outside.is_empty() {
        return format!("nothing changed since {}\n", choice.since());
    }
    let mut text = format!("since {}:\n", choice.since());
    let files = |files: &[String]| {
        let shown: Vec<_> = files.iter().take(3).map(String::as_str).collect();
        match files.len() - shown.len() {
            0 => shown.join(", "),
            more => format!("{} and {more} more", shown.join(", ")),
        }
    };
    if !choice.workspace.is_empty() {
        text += &format!(
            "  the whole workspace: {} changed\n",
            files(&choice.workspace)
        );
    }
    for chosen in &choice.packages {
        match &chosen.why {
            Why::Changed(changed) => {
                text += &format!("  {}: changed: {}\n", chosen.name, files(changed));
            }
            Why::Depends(on) => text += &format!("  {}: depends on {on}\n", chosen.name),
        }
    }
    if !choice.outside.is_empty() {
        text += &format!("  in no package: {}\n", files(&choice.outside));
    }
    if choice.is_empty() {
        text += "nothing to run\n";
    } else {
        let args: Vec<_> = choice
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        text += &format!("cargo arguments: {}\n", args.join(" "));
    }
    text
}

/// `cargo heli changed --json`'s document.
fn document(choice: &Choice, affected: bool) -> String {
    #[derive(Serialize)]
    struct Document<'a> {
        schema: u32,
        affected: bool,
        base: BaseDocument<'a>,
        workspace: &'a [String],
        packages: Vec<Package<'a>>,
        outside: &'a [String],
        args: Vec<String>,
    }
    #[derive(Serialize)]
    struct BaseDocument<'a> {
        reference: &'a str,
        commit: &'a str,
    }
    #[derive(Serialize)]
    struct Package<'a> {
        name: &'a str,
        why: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<&'a [String]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        depends_on: Option<&'a str>,
    }
    let document = Document {
        schema: SCHEMA,
        affected,
        base: BaseDocument {
            reference: &choice.base.reference,
            commit: &choice.base.commit,
        },
        workspace: &choice.workspace,
        packages: choice
            .packages
            .iter()
            .map(|chosen| match &chosen.why {
                Why::Changed(files) => Package {
                    name: &chosen.name,
                    why: "changed",
                    files: Some(files),
                    depends_on: None,
                },
                Why::Depends(on) => Package {
                    name: &chosen.name,
                    why: "dependent",
                    files: None,
                    depends_on: Some(on),
                },
            })
            .collect(),
        outside: &choice.outside,
        args: choice
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
    };
    serde_json::to_string(&document).expect("the selection serializes")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{Base, Member, Why, add_dependents, attribute, refusal};

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn base() -> Base {
        Base {
            reference: "main".to_owned(),
            commit: "0123456789".to_owned(),
            short: "0123456".to_owned(),
        }
    }

    /// leaf ← mid ← top, in `/ws/<name>`, and `/ws` the root.
    fn members() -> (PathBuf, Vec<Member>) {
        let root = Path::new(if cfg!(windows) { r"C:\ws" } else { "/ws" }).to_path_buf();
        let member = |name: &str, depends_on: Vec<usize>| Member {
            name: name.to_owned(),
            dir: root.join(name),
            depends_on,
        };
        let members = vec![
            member("leaf", vec![]),
            member("mid", vec![0]),
            member("top", vec![1]),
        ];
        (root, members)
    }

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// A file belongs to the member whose directory holds it; the
    /// dependents follow it up the graph; a root file selects the whole
    /// workspace; any other file selects nothing.
    #[test]
    fn files_select_their_members_and_dependents() {
        let (root, members) = members();
        let choice = attribute(&members, &root, &root, &files(&["leaf/src/lib.rs"]), base());
        assert_eq!(choice.args(), args(&["-p", "leaf"]));
        let mut affected = choice.packages.clone();
        add_dependents(&members, &mut affected);
        let names: Vec<_> = affected.iter().map(|chosen| &*chosen.name).collect();
        assert_eq!(names, ["leaf", "mid", "top"]);
        assert_eq!(affected[2].why, Why::Depends("mid".to_owned()));

        for whole in [
            "Cargo.lock",
            "Cargo.toml",
            ".cargo/config.toml",
            "rust-toolchain.toml",
        ] {
            let choice = attribute(&members, &root, &root, &files(&[whole]), base());
            assert_eq!(choice.args(), args(&["--workspace"]), "{whole}");
        }
        let choice = attribute(
            &members,
            &root,
            &root,
            &files(&["README.md", "mid/README.md"]),
            base(),
        );
        assert_eq!(choice.args(), args(&["-p", "mid"]));
        assert_eq!(choice.outside, ["README.md"]);
    }

    /// Only subcommands that take `-p` take a selection, and not with
    /// arguments that choose packages already.
    #[test]
    fn a_selection_is_refused_where_it_cannot_apply() {
        for ok in [
            &["clippy", "--all-targets", "--", "-D", "warnings"][..],
            &["+nightly", "test"],
            &["nextest", "run", "--", "-p"],
            &["nextest", "list"],
            &["llvm-cov", "nextest"],
        ] {
            assert_eq!(refusal("--changed", &args(ok)), None, "{ok:?}");
        }
        for refused in [
            &["build", "-p", "a"][..],
            &["check", "--package=a"],
            &["test", "-pa"],
            &["clippy", "--workspace"],
            &["build", "--all"],
            &["check", "--exclude", "a"],
            &["nextest", "run", "-E", "package(a)"],
            &["fmt"],
            &["run"],
            &["nextest", "archive"],
            &["llvm-cov", "report"],
        ] {
            assert!(
                refusal("--changed", &args(refused)).is_some(),
                "{refused:?}"
            );
        }
    }
}
