//! The command line: heli's own flags come before the subcommand, and
//! everything after it belongs to the tool, passed through verbatim.

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};

use crate::runlog;
use crate::runlog::select::Select;

/// What heli was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run a command.
    Run(Invocation),
    /// Show the daemon and its runs.
    Status { json: bool },
    /// Print the recorded output of run `run` (`r<number>`), or a part of it.
    Log { run: String, select: Select },
    /// Be the daemon, or with `detach`, start it outside this process tree.
    Daemon { detach: bool },
}

/// The program heli runs, and its arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run any command: `cargo heli exec -- <command> [args...]`.
    Exec {
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
    },
    /// Run heli's daemon in the foreground (`cargo heli` starts it itself).
    #[command(hide = true)]
    Daemon {
        /// Start the daemon in the background, outside this process's tree,
        /// and exit once it listens.
        #[arg(long)]
        detach: bool,
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
    Ok(match cli.command {
        Command::Exec { command } => {
            let mut command = command.into_iter();
            Action::Run(Invocation {
                program: command.next().expect("BUG: clap requires a command"),
                args: command.collect(),
            })
        }
        Command::Status { json } => Action::Status { json },
        Command::Log { run, tail, grep } => Action::Log {
            run,
            select: match (tail, grep) {
                (Some(count), _) => Select::Tail(count),
                (None, Some(pattern)) => Select::Grep(pattern),
                (None, None) => Select::All,
            },
        },
        Command::Daemon { detach } => Action::Daemon { detach },
        Command::Cargo(args) => Action::Run(Invocation {
            program: cargo,
            args,
        }),
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

/// A regular expression, checked here so that a bad one is a usage error.
fn parse_pattern(text: &str) -> Result<String, regex::Error> {
    regex::bytes::Regex::new(text).map(|_| text.to_owned())
}
