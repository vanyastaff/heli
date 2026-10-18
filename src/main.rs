//! `cargo heli`: cargo for machines shared by many agents (`DESIGN.md`).
//!
//! Modules, from the command line inward:
//! - [`cli`]: heli's command line, which gives each run its [`class`];
//! - [`client`]: hands runs to the daemon, or runs them [`direct`]ly when it
//!   cannot; asks it for its status; makes sure one runs (`ensure`);
//! - [`launch`]: starts the daemon in the background, through the
//!   [`autostart`] registration when setup made one;
//! - [`protocol`]: the versioned messages between client and daemon, over
//!   [`ipc`], the owner-only endpoint;
//! - [`daemon`]: the per-user daemon, its run registry
//!   ([`daemon::registry`]), run execution ([`daemon::execute`]), the
//!   result store ([`daemon::results`], keyed by [`daemon::key`]) and, on
//!   Unix, its pool's warden ([`daemon::warden`]);
//! - [`pool`]: the host pool, the jobserver every run draws from, sized by
//!   the per-user [`config`];
//! - [`status`]: the `status --json` schema, and where the daemon runs:
//!   its [`session`];
//! - [`changed`]: `--changed` and `--affected`, which choose a run's
//!   packages from what git says changed, and `cargo heli changed`;
//! - [`runlog`]: the record of each run's output, and `cargo heli log`;
//!   [`rerun`]: `cargo heli rerun`, which runs a recorded run again;
//! - [`output`]: what the client prints of a run, in the chosen output mode;
//! - [`store`]: the build store and its eviction ladder, [`gc`], which lists
//!   its directories and evicts, and [`recycle`], which git's
//!   `post-checkout` hook runs;
//! - [`setup`]: `cargo heli setup`, the environment that lets a bare cargo
//!   draw from the pool, cargo's `build-dir` in the build [`store`], and the
//!   [`autostart`] registration that starts the daemon at logon;
//! - [`locations`], [`worktree`], [`environment`], [`clock`], [`nesting`],
//!   [`run_id`], [`priority`]:
//!   what both sides share.

mod autostart;
mod changed;
mod class;
mod cli;
mod client;
mod clock;
mod config;
mod daemon;
mod dependencies;
mod dependency_home;
mod direct;
mod doctor;
mod environment;
mod gc;
mod ipc;
mod launch;
mod locations;
mod memory;
mod nesting;
mod output;
mod pool;
mod priority;
mod processes;
mod profiles;
mod protocol;
mod recycle;
mod rerun;
mod run_id;
mod runlog;
mod session;
mod setup;
mod source;
mod status;
mod store;
mod user_env;
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
    // This is an implementation process of an existing run, not another
    // nested heli invocation. It must work even at the nesting limit.
    if let cli::Action::LeaseRun {
        roots,
        command,
        dependency_epoch,
        direct,
    } = action
    {
        store::runner::main(&roots, &command, dependency_epoch.as_deref(), direct);
    }
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
        cli::Action::Ensure => client::ensure(depth),
        cli::Action::Stop { now, yes, restart } => client::stop(now, yes, restart.then_some(depth)),
        cli::Action::Setup { print, autostart } => setup::main(print, autostart),
        cli::Action::Changed {
            affected,
            base,
            json,
        } => changed::main(affected, base, json),
        cli::Action::Log { run, select } => client::log(run, &select),
        cli::Action::Cancel { run } => client::cancel(run),
        cli::Action::Rerun {
            run,
            failed,
            then_all,
        } => rerun::main(run, failed, then_all),
        cli::Action::Gc { dry_run, json } => gc::main(dry_run, json),
        cli::Action::Doctor { json } => doctor::main(json),
        cli::Action::Dependencies(command) => {
            let result = match command {
                cli::DependencyCommand::Import {
                    store,
                    vendor,
                    config,
                } => dependencies::import(&store, &vendor, &config).map(|path| {
                    println!("{}", path.display());
                }),
                cli::DependencyCommand::Gc { store } => dependencies::gc(&store).map(|report| {
                    println!("{report}");
                }),
            };
            if let Err(reason) = result {
                eprintln!("cargo-heli: {reason}");
                std::process::exit(REFUSED);
            }
        }
        cli::Action::Recycle => recycle::main(),
        cli::Action::Daemon { detach: false } => daemon::main(),
        cli::Action::Daemon { detach: true } => launch::detach(depth),
        cli::Action::Warden => daemon::warden::main(),
        cli::Action::LeaseRun { .. } => unreachable!("lease runner dispatched above"),
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
