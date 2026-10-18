//! The command line: heli's own flags come before the subcommand, and
//! everything after it belongs to the tool, passed through verbatim. The
//! subcommand and its arguments also give the run its class
//! (`crate::class`). heli's flags choose the output mode (`crate::output`):
//! `--output <mode>` or `-q`, before a cargo subcommand, or before or after
//! `exec`; and `--cache` / `--no-cache`, before a cargo subcommand.
//! `--changed[=REF]` and `--affected[=REF]`, before a cargo subcommand that
//! takes `-p`, choose its packages from what changed (`crate::changed`).

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::changed::Selection;
use crate::class::Class;
use crate::config::CargoHome;
use crate::output::Mode;
use crate::protocol::Cache;
use crate::run_id::RunId;
use crate::runlog::select::Select;

/// What heli was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run a command.
    Run(Invocation),
    /// Show the daemon and its runs.
    Status { json: bool },
    /// Start the daemon unless one runs.
    Ensure,
    /// Ask the daemon to exit once its runs end; with `now`, cancel them
    /// first (`yes`: without asking). With `restart`, then start a new one.
    Stop { now: bool, yes: bool, restart: bool },
    /// Offer the environment that lets a bare cargo draw from the pool;
    /// with `print`, only show it. `autostart` registers the daemon to start
    /// at logon (`Some(true)`), removes that registration (`Some(false)`),
    /// or leaves it as it is (`None`).
    Setup {
        print: bool,
        autostart: Option<bool>,
    },
    /// Print what `--changed`, or with `affected` `--affected`, would select
    /// since `base`, and why; as JSON with `json`.
    Changed {
        affected: bool,
        base: Option<String>,
        json: bool,
    },
    /// Print the recorded output of run `run` (`r<number>`), or a part of it.
    Log { run: RunId, select: Select },
    /// Kill run `run` (`r<number>`) with every process it started.
    Cancel { run: RunId },
    /// Run run `run` again, or the latest failed test run in this worktree
    /// (`crate::rerun`): whole, or with `failed` only its failed tests,
    /// and with `then_all` the whole run once those pass.
    Rerun {
        run: Option<RunId>,
        failed: bool,
        then_all: bool,
    },
    /// List the build store's directories and evict what the ladder says,
    /// or with `dry_run` only name it; as JSON with `json`. With
    /// `superseded`, reclaim every superseded directory instead.
    Gc {
        dry_run: bool,
        json: bool,
        superseded: bool,
    },
    /// Explain how the installed cargo tools and the cargo configuration
    /// interact with heli; as JSON with `json`.
    Doctor { json: bool },
    /// Import or collect managed immutable dependency snapshots.
    Dependencies(DependencyCommand),
    /// Give the worktree here a removed worktree's build directory, as
    /// git's `post-checkout` hook asks.
    Recycle,
    /// Be the daemon, or with `detach`, start it outside this process tree.
    Daemon { detach: bool },
    /// Be the daemon's pool warden.
    Warden,
    /// Internal parent retaining runtime leases while the actual command runs.
    LeaseRun {
        roots: Vec<std::path::PathBuf>,
        command: Vec<OsString>,
        dependency_epoch: Option<std::path::PathBuf>,
        direct: bool,
    },
}

/// The program heli runs, its arguments, the class of the run, and how to
/// show its output.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub class: Class,
    /// Whether `cargo heli exec` runs it; else it is a cargo subcommand.
    pub exec: bool,
    /// How the run uses the result store.
    pub cache: Cache,
    /// The output mode heli's flags chose, if they chose one.
    pub output: Option<Mode>,
    /// Explicit immutable dependency snapshot; inherited by nested Heli.
    pub dependency_epoch: Option<std::path::PathBuf>,
    pub cargo_home: Option<CargoHome>,
    /// heli's `--no-cache` and `--cache` flags, for [`Invocation::reclassify`].
    pub cache_flags: (bool, Option<Cache>),
    /// Where its class came from, once the client has read the profiles
    /// (`crate::profiles::Source::name`).
    pub profile: Option<&'static str>,
    /// heli's command line as the user typed it, after `cargo heli`, for
    /// the run's record (`crate::runlog::Start`).
    pub typed: Vec<OsString>,
    /// The run this one reruns: `cargo heli rerun` runs heli again with the
    /// hidden `--rerun-of <run>` first.
    pub rerun_of: Option<RunId>,
    /// The packages `--changed` or `--affected` choose, which the client
    /// adds to `args` (`crate::changed::apply`).
    pub select: Option<Selection>,
}

impl Invocation {
    /// Gives a cargo subcommand's run `class`, as a tool profile says
    /// (`crate::profiles`), and the result-store use that follows from it
    /// and heli's flags.
    pub fn reclassify(&mut self, class: Class) {
        if self.exec || class == self.class {
            return;
        }
        self.class = class;
        let (no_cache, flag) = self.cache_flags;
        self.cache = cargo_cache(class, &self.args, no_cache, flag);
    }
}

/// How a run of `class`, `cargo <args>`, uses the result store, from heli's
/// `--no-cache` and `--cache` flags.
fn cargo_cache(class: Class, args: &[OsString], no_cache: bool, flag: Option<Cache>) -> Cache {
    match Cache::of(class, args, no_cache) {
        Cache::Off => Cache::Off,
        // A successful build is not a reusable verdict: each
        // artifact owner must get its actual outputs.
        _ if class == Class::Compile => Cache::Live,
        default => flag.unwrap_or(default),
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "cargo-heli",
    bin_name = "cargo heli",
    version,
    about = "Run cargo on a machine shared by many agents",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(flatten)]
    output: OutputFlags,
    /// Run even when the same inputs already passed; the run is recorded
    /// when it passes.
    #[arg(long)]
    no_cache: bool,
    /// Result cache for checks/tests: on (reuse and record), refresh (record
    /// only), off (share matching live checks without saved verdicts).
    #[arg(long, value_name = "MODE", value_parser = parse_cache, conflicts_with = "no_cache")]
    cache: Option<Cache>,
    /// Use an imported immutable dependency snapshot and a private Cargo home.
    #[arg(long, value_name = "PATH")]
    dependency_epoch: Option<std::path::PathBuf>,
    /// Writable Cargo home: isolated per worktree, or the caller's shared home.
    #[arg(long, value_name = "MODE", value_parser = parse_cargo_home)]
    cargo_home: Option<CargoHome>,
    /// The run this one reruns, as `cargo heli rerun` says (first of all).
    #[arg(long, hide = true, value_parser = parse_run)]
    rerun_of: Option<RunId>,
    /// Run on the workspace members whose files changed since the merge base
    /// with REF (default: the upstream branch, else origin/HEAD), as `-p`.
    #[arg(long, value_name = "REF", num_args = 0..=1, require_equals = true)]
    changed: Option<Option<String>>,
    /// As --changed, and on every member that depends on those too.
    #[arg(
        long,
        value_name = "REF",
        num_args = 0..=1,
        require_equals = true,
        conflicts_with = "changed"
    )]
    affected: Option<Option<String>>,
    #[command(subcommand)]
    command: Command,
}

/// The flags that choose the output mode.
#[derive(Debug, Args)]
struct OutputFlags {
    /// How much of the tool's output to print: full (as the tool prints
    /// it), compact (what an agent needs), or quiet (one line on success).
    /// Default: `HELI_OUTPUT`, the configuration, else full on a terminal and
    /// compact without.
    #[arg(long, value_name = "MODE", value_parser = parse_mode)]
    output: Option<Mode>,
    /// Quiet output: `--output quiet`.
    #[arg(short = 'q', conflicts_with = "output")]
    quiet: bool,
}

impl OutputFlags {
    fn mode(&self) -> Option<Mode> {
        if self.quiet {
            Some(Mode::Quiet)
        } else {
            self.output
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(hide = true)]
    LeaseRun {
        #[arg(long)]
        direct: bool,
        #[arg(long)]
        dependency_epoch: Option<std::path::PathBuf>,
        #[arg(long = "root", required = true)]
        roots: Vec<std::path::PathBuf>,
        #[arg(last = true, required = true)]
        command: Vec<OsString>,
    },
    /// Manage immutable Cargo dependency snapshots (opt-in, Cargo 1.99+).
    Deps {
        #[command(subcommand)]
        command: DependencyCommand,
    },
    /// Run any command: `cargo heli exec -- <command> [args...]`.
    Exec {
        #[command(flatten)]
        output: OutputFlags,
        /// Declare a read-only verification: share only matching source and
        /// configuration inputs. Defaults to no saved result cache.
        #[arg(long)]
        verify: bool,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Show the daemon and the runs in flight.
    Status {
        /// Print the versioned JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Start heli's daemon unless one is running; for a session-start hook.
    Ensure,
    /// Ask heli's daemon to exit once its runs in flight have ended; it
    /// takes no new runs meanwhile.
    Stop {
        /// Cancel the runs in flight instead of waiting for them, after a
        /// confirmation.
        #[arg(long)]
        now: bool,
        /// Cancel without asking.
        #[arg(long, short, requires = "now")]
        yes: bool,
    },
    /// Replace heli's daemon with one of this cargo-heli: stop it, wait
    /// until it has exited, and start a new one.
    Restart {
        /// Cancel the runs in flight instead of waiting for them, after a
        /// confirmation.
        #[arg(long)]
        now: bool,
        /// Cancel without asking.
        #[arg(long, short, requires = "now")]
        yes: bool,
    },
    /// Offer to set `CARGO_MAKEFLAGS` for agents and shells, so that a bare
    /// cargo draws from heli's pool; each file is written only on a yes.
    Setup {
        /// Only show what would be written; ask nothing, write nothing.
        #[arg(long)]
        print: bool,
        /// Also register the daemon to start when you log on, in your
        /// session: a Task Scheduler task, a systemd user unit or a launchd
        /// agent. Clients then start it through that registration.
        #[arg(long, conflicts_with = "no_autostart")]
        autostart: bool,
        /// Remove the registration `--autostart` made.
        #[arg(long)]
        no_autostart: bool,
    },
    /// Print which workspace members changed since a base, and why: what
    /// `--changed` (or `--affected`) would run on.
    Changed {
        /// Also the members that depend on the changed ones.
        #[arg(long)]
        affected: bool,
        /// Compare with the merge base of HEAD and REF (default: the upstream
        /// branch, else origin/HEAD).
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// Print the versioned JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Print a run's recorded output: stdout on stdout, stderr on stderr.
    Log {
        /// The run, as the footer after it and `status` name it: `r17` (or
        /// `17`).
        #[arg(value_parser = parse_run)]
        run: RunId,
        /// Only the last N lines, of both streams together.
        #[arg(long, value_name = "N", group = "select")]
        tail: Option<usize>,
        /// Only the lines that match this regular expression.
        #[arg(long, value_name = "RE", group = "select", value_parser = parse_pattern)]
        grep: Option<String>,
        /// Only the errors, as the compiler rendered them, all of them.
        #[arg(long, group = "select")]
        errors: bool,
        /// Only one test's output: `tests::it_works`, or nextest's binary
        /// and name, `my-crate tests::it_works`.
        #[arg(long, value_name = "NAME", group = "select")]
        test: Option<String>,
        /// All of it byte for byte, cargo's JSON messages included, instead
        /// of as a person would have seen it.
        #[arg(long, group = "select")]
        raw: bool,
    },
    /// Kill a run in flight with every process it started; its clients
    /// report it as cancelled.
    Cancel {
        /// The run, as the footer after it and `status` name it: `r17` (or
        /// `17`).
        #[arg(value_parser = parse_run)]
        run: RunId,
    },
    /// Run a recorded run again, from its directory, with the current
    /// environment: by default the latest failed test run in this worktree.
    Rerun {
        /// The run, as the footer after it and `status` name it: `r17` (or
        /// `17`).
        #[arg(value_parser = parse_run)]
        run: Option<RunId>,
        /// Only the tests that failed in it.
        #[arg(long)]
        failed: bool,
        /// Then, once those pass, the whole run.
        #[arg(long, requires = "failed")]
        then_all: bool,
    },
    /// List every directory of the build store, with its state, and evict
    /// what the ladder chooses when the store is over its budget or a
    /// volume under its floor.
    Gc {
        /// Only list the directories and the victims, each with its tier
        /// and reason, and delete nothing.
        #[arg(long)]
        dry_run: bool,
        /// Print the versioned JSON document instead of text.
        #[arg(long)]
        json: bool,
        /// Reclaim, whatever the budget, every directory of intermediates in
        /// a registered target/ directory that the build store superseded,
        /// and nothing else.
        #[arg(long)]
        superseded: bool,
    },
    /// Explain how the installed cargo tools and the cargo configuration
    /// (`jobs`, `target-dir`, `build-dir`, wrappers, linkers) interact with
    /// heli, and what to do about it.
    Doctor {
        /// Print the versioned JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Give the worktree here the build directory of a removed worktree of
    /// its repository, if it has none (run by the `post-checkout` hook
    /// `cargo heli setup` installs).
    #[command(hide = true)]
    Recycle,
    /// Run heli's daemon in the foreground (`cargo heli` starts it itself).
    #[command(hide = true)]
    Daemon {
        /// Start the daemon in the background, outside this process's tree,
        /// and exit once it listens.
        #[arg(long)]
        detach: bool,
        /// Be the pool's warden, which the daemon starts: once the daemon at
        /// the other end of stdin is gone, remove its pool when nobody holds
        /// it.
        #[arg(long, hide = true, conflicts_with = "detach")]
        warden: bool,
    },
    /// Any cargo subcommand, e.g. `cargo heli build -p foo`: run as
    /// `cargo build -p foo`.
    #[command(external_subcommand)]
    Cargo(Vec<OsString>),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum DependencyCommand {
    /// Import a cargo vendor tree; print its immutable snapshot path.
    Import {
        #[arg(long)]
        store: std::path::PathBuf,
        #[arg(long)]
        vendor: std::path::PathBuf,
        /// Source replacement TOML emitted by cargo vendor.
        #[arg(long)]
        config: std::path::PathBuf,
    },
    /// Collect all unleased snapshots and unreferenced package objects.
    Gc {
        #[arg(long)]
        store: std::path::PathBuf,
    },
}

/// Parses heli's command line (`args` as the process received it) into what
/// to do; `cargo` is the cargo program a cargo subcommand runs with.
///
/// Run as `cargo heli`, the binary gets `heli` as its first argument; it is
/// dropped, so `cargo heli build` and `cargo-heli build` mean the same.
pub fn parse(
    args: impl IntoIterator<Item = OsString>,
    cargo: OsString,
) -> Result<Action, clap::Error> {
    let mut args = args.into_iter().peekable();
    let binary = args.next();
    if args.peek().is_some_and(|first| first == "heli") {
        args.next();
    }
    let args: Vec<OsString> = args.collect();
    let typed = match args.first() {
        Some(first) if first == "--rerun-of" => args.get(2..).unwrap_or_default().to_vec(),
        Some(first) if first.as_encoded_bytes().starts_with(b"--rerun-of=") => args[1..].to_vec(),
        _ => args.clone(),
    };
    let cli = Cli::try_parse_from(binary.into_iter().chain(args))?;
    if let Command::Cargo(args) = &cli.command
        && args.first().is_some_and(|subcommand| subcommand == "heli")
    {
        return Err(Cli::command().error(
            ErrorKind::InvalidSubcommand,
            "`heli` is heli itself: `cargo heli heli …` would run heli again",
        ));
    }
    let output = cli.output.mode();
    let select = match (&cli.changed, &cli.affected) {
        (Some(base), None) => Some(Selection {
            affected: false,
            base: base.clone().filter(|base| !base.is_empty()),
        }),
        (None, Some(base)) => Some(Selection {
            affected: true,
            base: base.clone().filter(|base| !base.is_empty()),
        }),
        _ => None,
    };
    if let Some(selection) = &select {
        let flag = if selection.affected {
            "--affected"
        } else {
            "--changed"
        };
        let refusal = match &cli.command {
            Command::Cargo(args) => crate::changed::refusal(flag, args),
            _ => Some(format!("{flag} goes only before a cargo subcommand")),
        };
        if let Some(refusal) = refusal {
            return Err(Cli::command().error(ErrorKind::ArgumentConflict, refusal));
        }
    }
    Ok(match cli.command {
        Command::LeaseRun {
            roots,
            command,
            dependency_epoch,
            direct,
        } => Action::LeaseRun {
            roots,
            command,
            dependency_epoch,
            direct,
        },
        Command::Deps { command } => Action::Dependencies(command),
        Command::Exec {
            output: own,
            verify,
            command,
        } => {
            let mut command = command.into_iter();
            Action::Run(Invocation {
                program: command.next().expect("BUG: clap requires a command"),
                args: command.collect(),
                class: if verify { Class::Verify } else { Class::Exec },
                exec: true,
                cache: if verify {
                    if cli.no_cache {
                        Cache::Refresh
                    } else {
                        cli.cache.unwrap_or(Cache::Live)
                    }
                } else {
                    Cache::Off
                },
                output: own.mode().or(output),
                dependency_epoch: cli.dependency_epoch,
                cargo_home: cli.cargo_home,
                cache_flags: (cli.no_cache, cli.cache),
                profile: None,
                typed,
                rerun_of: cli.rerun_of,
                select: None,
            })
        }
        Command::Status { json } => Action::Status { json },
        Command::Ensure => Action::Ensure,
        Command::Stop { now, yes } => Action::Stop {
            now,
            yes,
            restart: false,
        },
        Command::Restart { now, yes } => Action::Stop {
            now,
            yes,
            restart: true,
        },
        Command::Gc {
            dry_run,
            json,
            superseded,
        } => Action::Gc {
            dry_run,
            json,
            superseded,
        },
        Command::Doctor { json } => Action::Doctor { json },
        Command::Setup {
            print,
            autostart,
            no_autostart,
        } => Action::Setup {
            print,
            autostart: match (autostart, no_autostart) {
                (true, _) => Some(true),
                (false, true) => Some(false),
                (false, false) => None,
            },
        },
        Command::Changed {
            affected,
            base,
            json,
        } => Action::Changed {
            affected,
            base,
            json,
        },
        Command::Log {
            run,
            tail,
            grep,
            errors,
            test,
            raw,
        } => Action::Log {
            run,
            select: match (tail, grep, test) {
                (Some(count), ..) => Select::Tail(count),
                (None, Some(pattern), _) => Select::Grep(pattern),
                (None, None, Some(name)) => Select::Test(name),
                (None, None, None) if errors => Select::Errors,
                (None, None, None) if raw => Select::Raw,
                (None, None, None) => Select::All,
            },
        },
        Command::Cancel { run } => Action::Cancel { run },
        Command::Rerun {
            run,
            failed,
            then_all,
        } => Action::Rerun {
            run,
            failed,
            then_all,
        },
        Command::Recycle => Action::Recycle,
        Command::Daemon { warden: true, .. } => Action::Warden,
        Command::Daemon { detach, .. } => Action::Daemon { detach },
        Command::Cargo(args) => {
            let class = Class::of_cargo(&args);
            Action::Run(Invocation {
                program: cargo_for(&args, cargo),
                class,
                exec: false,
                cache: cargo_cache(class, &args, cli.no_cache, cli.cache),
                args,
                output,
                dependency_epoch: cli.dependency_epoch,
                cargo_home: cli.cargo_home,
                cache_flags: (cli.no_cache, cli.cache),
                profile: None,
                typed,
                rerun_of: cli.rerun_of,
                select,
            })
        }
    })
}

/// The cargo that runs `cargo <args>`: `cargo`, the one that started heli,
/// unless `args` start with a `+toolchain` and `cargo` is a toolchain's own
/// binary (`.../toolchains/<name>/bin/cargo`), as cargo hands its external
/// subcommands: that binary does not take `+toolchain`, rustup's proxy does,
/// so `cargo` on the search path runs it. Any other `cargo` is kept.
fn cargo_for(args: &[OsString], cargo: OsString) -> OsString {
    let pinned = args
        .first()
        .is_some_and(|first| first.as_encoded_bytes().starts_with(b"+"));
    let components: Vec<_> = std::path::Path::new(&cargo).components().collect();
    let in_a_toolchain = components
        .windows(4)
        .any(|parts| parts[0].as_os_str() == "toolchains" && parts[2].as_os_str() == "bin");
    if pinned && in_a_toolchain {
        OsString::from("cargo")
    } else {
        cargo
    }
}

fn parse_cache(text: &str) -> Result<Cache, String> {
    match text {
        "on" => Ok(Cache::On),
        "refresh" => Ok(Cache::Refresh),
        "off" => Ok(Cache::Live),
        _ => Err("cache mode must be on, refresh or off".to_owned()),
    }
}

fn parse_cargo_home(text: &str) -> Result<CargoHome, String> {
    match text {
        "shared" => Ok(CargoHome::Shared),
        "isolated" => Ok(CargoHome::Isolated),
        _ => Err(format!(
            "unknown Cargo home mode {text:?}; expected isolated or shared"
        )),
    }
}

/// A run id as the user gives it, `r17` or `17`, as heli writes it: `r17`.
fn parse_run(text: &str) -> Result<RunId, String> {
    let spelled = if text.starts_with('r') {
        text.to_owned()
    } else {
        format!("r{text}")
    };
    spelled
        .parse()
        .map_err(|_| "a run id is r<number>, as the footer after a run names it".to_owned())
}

/// An output mode, checked here so that a bad one is a usage error.
fn parse_mode(text: &str) -> Result<Mode, String> {
    text.parse()
}

/// A regular expression, checked here so that a bad one is a usage error.
fn parse_pattern(text: &str) -> Result<String, regex::Error> {
    regex::bytes::Regex::new(text).map(|_| text.to_owned())
}

#[cfg(test)]
mod cargo_tests {
    use std::ffi::OsString;

    use super::cargo_for;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// A `+toolchain` goes to rustup's proxy when heli was started by a
    /// toolchain's own cargo, which cannot take it; otherwise the cargo is
    /// kept.
    #[test]
    fn a_pinned_toolchain_goes_through_rustups_proxy() {
        let own =
            OsString::from("/home/u/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo");
        assert_eq!(
            cargo_for(&args(&["+nightly", "udeps"]), own.clone()),
            "cargo"
        );
        assert_eq!(cargo_for(&args(&["udeps"]), own.clone()), own);
        let fake = OsString::from("/tmp/fixture/fake-cargo/cargo");
        assert_eq!(cargo_for(&args(&["+pinned", "clippy"]), fake.clone()), fake);
    }
}
