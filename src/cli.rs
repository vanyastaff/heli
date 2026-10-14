//! The command line: heli's own flags come before the subcommand, and
//! everything after it belongs to the tool, passed through verbatim. The
//! subcommand and its arguments also give the run its class
//! (`crate::class`). heli's flags choose the output mode (`crate::output`):
//! `--output <mode>` or `-q`, before a cargo subcommand, or before or after
//! `exec`; and `--no-cache`, before a cargo subcommand.

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::class::Class;
use crate::daemon::results;
use crate::output::Mode;
use crate::protocol::Cache;
use crate::runlog;
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
    /// Offer the environment that lets a bare cargo draw from the pool;
    /// with `print`, only show it. `autostart` registers the daemon to start
    /// at logon (`Some(true)`), removes that registration (`Some(false)`),
    /// or leaves it as it is (`None`).
    Setup {
        print: bool,
        autostart: Option<bool>,
    },
    /// Print the recorded output of run `run` (`r<number>`), or a part of it.
    Log { run: String, select: Select },
    /// Kill run `run` (`r<number>`) with every process it started.
    Cancel { run: String },
    /// List the build store's directories and evict what the ladder says,
    /// or with `dry_run` only name it; as JSON with `json`.
    Gc { dry_run: bool, json: bool },
    /// Be the daemon, or with `detach`, start it outside this process tree.
    Daemon { detach: bool },
    /// Be the daemon's pool warden.
    Warden,
}

/// The program heli runs, its arguments, the class of the run, and how to
/// show its output.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub class: Class,
    /// How the run uses the result store.
    pub cache: Cache,
    /// The output mode heli's flags chose, if they chose one.
    pub output: Option<Mode>,
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
    /// Run any command: `cargo heli exec -- <command> [args...]`.
    Exec {
        #[command(flatten)]
        output: OutputFlags,
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
    /// Print a run's recorded output: stdout on stdout, stderr on stderr.
    Log {
        /// The run, as the footer after it and `status` name it: `r17` (or
        /// `17`).
        #[arg(value_parser = parse_run)]
        run: String,
        /// Only the last N lines, of both streams together.
        #[arg(long, value_name = "N", group = "select")]
        tail: Option<usize>,
        /// Only the lines that match this regular expression.
        #[arg(long, value_name = "RE", group = "select", value_parser = parse_pattern)]
        grep: Option<String>,
        /// Only the errors, as the compiler rendered them, all of them.
        #[arg(long, group = "select")]
        errors: bool,
    },
    /// Kill a run in flight with every process it started; its clients
    /// report it as cancelled.
    Cancel {
        /// The run, as the footer after it and `status` name it: `r17` (or
        /// `17`).
        #[arg(value_parser = parse_run)]
        run: String,
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
    },
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
    Ok(match cli.command {
        Command::Exec {
            output: own,
            command,
        } => {
            let mut command = command.into_iter();
            Action::Run(Invocation {
                program: command.next().expect("BUG: clap requires a command"),
                args: command.collect(),
                class: Class::Exec,
                cache: Cache::Off,
                output: own.mode().or(output),
            })
        }
        Command::Status { json } => Action::Status { json },
        Command::Ensure => Action::Ensure,
        Command::Gc { dry_run, json } => Action::Gc { dry_run, json },
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
        Command::Log {
            run,
            tail,
            grep,
            errors,
        } => Action::Log {
            run,
            select: match (tail, grep) {
                (Some(count), _) => Select::Tail(count),
                (None, Some(pattern)) => Select::Grep(pattern),
                (None, None) if errors => Select::Errors,
                (None, None) => Select::All,
            },
        },
        Command::Cancel { run } => Action::Cancel { run },
        Command::Daemon { warden: true, .. } => Action::Warden,
        Command::Daemon { detach, .. } => Action::Daemon { detach },
        Command::Cargo(args) => {
            let class = Class::of_cargo(&args);
            Action::Run(Invocation {
                program: cargo,
                class,
                cache: results::cache(class, &args, cli.no_cache),
                args,
                output,
            })
        }
    })
}

/// A run id as the user gives it, `r17` or `17`, as heli writes it: `r17`.
fn parse_run(text: &str) -> Result<String, String> {
    let run = if text.starts_with('r') {
        text.to_owned()
    } else {
        format!("r{text}")
    };
    runlog::run_number(&run)
        .map(|number| format!("r{number}"))
        .ok_or_else(|| "a run id is r<number>, as the footer after a run names it".to_owned())
}

/// An output mode, checked here so that a bad one is a usage error.
fn parse_mode(text: &str) -> Result<Mode, String> {
    text.parse()
}

/// A regular expression, checked here so that a bad one is a usage error.
fn parse_pattern(text: &str) -> Result<String, regex::Error> {
    regex::bytes::Regex::new(text).map(|_| text.to_owned())
}
