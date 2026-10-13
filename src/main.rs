//! `cargo heli`: cargo for machines shared by many agents (`DESIGN.md`).
//!
//! Modules, from the command line inward:
//! - [`cli`]: heli's command line;
//! - [`client`]: hands runs to the daemon, or runs them [`direct`]ly when it
//!   cannot; asks it for its status;
//! - [`launch`]: starts the daemon in the background;
//! - [`protocol`]: the versioned messages between client and daemon, over
//!   [`ipc`], the owner-only endpoint;
//! - [`daemon`]: the per-user daemon, its run registry
//!   ([`daemon::registry`]) and run execution ([`daemon::execute`]);
//! - [`status`]: the `status --json` schema;
//! - [`locations`], [`worktree`], [`environment`], [`clock`], [`nesting`]:
//!   what both sides share.

mod cli;
mod client;
mod clock;
mod daemon;
mod direct;
mod environment;
mod ipc;
mod launch;
mod locations;
mod nesting;
mod protocol;
mod status;
mod worktree;

use std::ffi::OsString;

/// Exit code for heli's own refusals, the same as for a usage error.
const REFUSED: i32 = 2;

fn main() {
    let action = match cli::parse(std::env::args_os(), cargo_program()) {
        Ok(action) => action,
        Err(error) => error.exit(),
    };
    let depth = nesting::current();
    if depth >= nesting::MAX {
        eprintln!(
            "cargo-heli: refusing to run: heli is already nested {depth} levels deep \
             ({}={depth}); does the command run heli again?",
            nesting::VAR
        );
        std::process::exit(REFUSED);
    }
    match action {
        cli::Action::Run(invocation) => client::run(invocation, depth),
        cli::Action::Status { json } => client::status(json),
        cli::Action::Daemon { detach: false } => daemon::main(),
        cli::Action::Daemon { detach: true } => launch::detach(depth),
    }
}

/// The cargo that started heli (`CARGO`, which cargo sets for its external
/// subcommands, so `cargo +nightly heli` keeps nightly), else `cargo` on the
/// search path.
fn cargo_program() -> OsString {
    std::env::var_os("CARGO")
        .filter(|cargo| !cargo.is_empty())
        .unwrap_or_else(|| OsString::from("cargo"))
}
