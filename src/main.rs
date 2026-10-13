//! `cargo heli`: cargo for machines shared by many agents (`DESIGN.md`).
//!
//! For now every command runs directly, as if heli were not there: the
//! daemon, the host pool and the rest of the design build on this entry
//! point.

mod cli;
mod direct;
mod nesting;

use std::ffi::OsString;
use std::process::Command;

/// Exit code for heli's own refusals, the same as for a usage error.
const REFUSED: i32 = 2;

fn main() {
    let invocation = match cli::parse(std::env::args_os(), cargo_program()) {
        Ok(invocation) => invocation,
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
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .env(nesting::VAR, (depth + 1).to_string());
    direct::run(command)
}

/// The cargo that started heli (`CARGO`, which cargo sets for its external
/// subcommands, so `cargo +nightly heli` keeps nightly), else `cargo` on the
/// search path.
fn cargo_program() -> OsString {
    std::env::var_os("CARGO")
        .filter(|cargo| !cargo.is_empty())
        .unwrap_or_else(|| OsString::from("cargo"))
}
