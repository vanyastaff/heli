//! The command line: heli's own flags come before the subcommand, and
//! everything after it belongs to the tool, passed through verbatim.

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};

use crate::protocol::Cache;

/// What heli was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Run a command.
    Run(Invocation),
    /// Show the daemon and its runs.
    Status { json: bool },
    /// Be the daemon, or with `detach`, start it outside this process tree.
    Daemon { detach: bool },
}

/// The program heli runs, and its arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// How the run uses the result store.
    pub cache: Cache,
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
    /// Run even when the same inputs already passed; the run is recorded
    /// when it passes.
    #[arg(long)]
    no_cache: bool,
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
                cache: Cache::Off,
            })
        }
        Command::Status { json } => Action::Status { json },
        Command::Daemon { detach } => Action::Daemon { detach },
        Command::Cargo(args) => Action::Run(Invocation {
            program: cargo,
            cache: match (verifies(&args), cli.no_cache) {
                (false, _) => Cache::Off,
                (true, false) => Cache::On,
                (true, true) => Cache::Refresh,
            },
            args,
        }),
    })
}

/// Whether the cargo subcommand `args` is a verify or a test run, the runs
/// whose verdict the result store keeps: `check`, `clippy` (not `--fix`),
/// `fmt --check`, `test` and `nextest run`. A request for help is not.
///
/// A stand-in until the run classes (DESIGN.md §5, "Classes") exist: they
/// replace this function.
fn verifies(args: &[OsString]) -> bool {
    let tool_args: Vec<&OsString> = args.iter().skip(1).take_while(|arg| *arg != "--").collect();
    let has = |flag: &str| tool_args.iter().any(|arg| *arg == flag);
    if has("-h") || has("--help") {
        return false;
    }
    let Some(subcommand) = args.first().and_then(|first| first.to_str()) else {
        return false;
    };
    match subcommand {
        "check" | "c" | "test" | "t" => true,
        "clippy" => !has("--fix"),
        "fmt" => has("--check"),
        "nextest" => tool_args.first().is_some_and(|arg| *arg == "run"),
        _ => false,
    }
}
