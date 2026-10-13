//! The command line: heli's own flags come before the subcommand, and
//! everything after it belongs to the tool, passed through verbatim.

use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};

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
    /// Any cargo subcommand, e.g. `cargo heli build -p foo`: run as
    /// `cargo build -p foo`.
    #[command(external_subcommand)]
    Cargo(Vec<OsString>),
}

/// Parses heli's command line (`args` as the process received it) into what
/// to run; `cargo` is the cargo program a cargo subcommand runs with.
///
/// Run as `cargo heli`, the binary gets `heli` as its first argument; it is
/// dropped, so `cargo heli build` and `cargo-heli build` mean the same.
pub fn parse(
    args: impl IntoIterator<Item = OsString>,
    cargo: OsString,
) -> Result<Invocation, clap::Error> {
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
            Invocation {
                program: command.next().expect("BUG: clap requires a command"),
                args: command.collect(),
            }
        }
        Command::Cargo(args) => Invocation {
            program: cargo,
            args,
        },
    })
}
