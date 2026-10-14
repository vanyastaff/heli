//! `cargo heli setup` (DESIGN.md §4, "Reach"): offers the environment that
//! makes a bare cargo draw from the host pool, and writes it where agents
//! and shells pick it up, each file only once the user has agreed.
//!
//! - **The environment** is [`VAR`], pointing at the host pool exactly as
//!   the daemon points every run at it (`crate::pool::makeflags`). It names
//!   the pool, not a daemon: with no daemon running, cargo cannot open the
//!   pool, warns, and builds with a pool of its own.
//! - **Where**: Claude Code's user settings (`env` in `settings.json`,
//!   [`claude`]), when Claude Code is set up for this user, and the profile
//!   of the shell `SHELL` names ([`profile`]). A file that already sets the
//!   variable to this value is left as it is, and so is one setup cannot
//!   edit safely (settings that are not a JSON object), with the reason.
//! - **Consent.** For each file, setup first shows exactly what it would
//!   write: the file's whole new content ("would become:"), or the lines the
//!   file would end with ("would end with:"), each followed by an empty
//!   line. It then asks `Write <path>? [y/N]` and reads the answer from
//!   stdin. Only `y` or `yes` writes; anything else, the end of input, or no
//!   input at all, leaves the file as it was. setup exits 1 when a file it
//!   was told to write cannot be written.
//! - **`--print`** shows the same and asks nothing, so it writes nothing.
//!
//! Another file setup comes to manage (the build store's `build-dir` in the
//! Cargo configuration) is one more [`Offer`].

mod claude;
mod profile;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use crate::locations::Locations;
use crate::pool;

/// The variable that hands a cargo the host pool.
pub const VAR: &str = "CARGO_MAKEFLAGS";

/// `cargo heli setup [--print]`.
pub fn main(print: bool) -> ! {
    let makeflags = match Locations::resolve()
        .and_then(|locations| pool::makeflags(&locations.pool).map_err(|error| error.to_string()))
    {
        Ok(makeflags) => makeflags,
        Err(reason) => {
            eprintln!("cargo-heli: cannot name the host pool: {reason}");
            std::process::exit(1)
        }
    };
    println!("{VAR}={makeflags}");
    println!(
        "With it, a bare cargo draws from heli's host pool; while no heli daemon runs, \
         cargo warns and builds on its own."
    );

    let claude = claude::settings().map(|path| Offer {
        what: "Claude Code settings",
        edit: claude::edit(&path, &makeflags),
        path,
    });
    let profile = profile::locate()
        .map(|(path, syntax)| Offer {
            what: "Shell profile",
            edit: profile::edit(&path, syntax, &makeflags),
            path,
        })
        .ok_or_else(|| profile::hint(&makeflags));
    let mut consent = if print {
        Consent::Never
    } else {
        let input = io::stdin();
        Consent::Ask {
            echo: !input.is_terminal(),
            input: input.lock(),
        }
    };
    let mut failed = false;
    for target in [claude, profile] {
        println!();
        match target {
            Ok(offer) => failed |= !offer.present(&mut consent),
            Err(note) => println!("{note}"),
        }
    }
    println!(
        "\nTo start the daemon as an agent session starts, run `cargo heli ensure` \
         from the harness's session-start hook."
    );
    std::process::exit(i32::from(failed))
}

/// A file setup offers to edit.
struct Offer {
    /// What the file is, for people.
    what: &'static str,
    path: PathBuf,
    edit: Edit,
}

/// What setup would do to a file.
enum Edit {
    /// Nothing: the file already sets the variable to the value.
    Done,
    /// Nothing: the file cannot be edited safely, for this reason.
    Refused(String),
    /// Write `content` as the file's whole content, shown as `shown` says.
    Write { content: String, shown: Shown },
}

/// How an [`Edit::Write`] is shown.
enum Shown {
    /// The file's whole new content.
    Whole,
    /// The lines the file would end with.
    Ending(String),
}

/// Where the yes or no to each file comes from.
enum Consent {
    /// `--print`: nothing is asked, so nothing is written.
    Never,
    /// A line of `input` answers each question; with `echo`, the answer is
    /// shown, as no terminal shows it.
    Ask {
        input: io::StdinLock<'static>,
        echo: bool,
    },
}

impl Consent {
    /// Reads the answer to the question just asked: true on `y` or `yes`.
    fn agrees(&mut self) -> bool {
        let Self::Ask { input, echo } = self else {
            return false;
        };
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let read = input.read_line(&mut answer).unwrap_or(0);
        // What PowerShell pipes in can start with a byte order mark.
        let answer = answer.trim_start_matches('\u{feff}').trim();
        if read == 0 || *echo {
            println!("{answer}");
        }
        matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

impl Offer {
    /// Shows the edit, asks whether to make it unless `consent` is never
    /// given, and makes it on a yes. False when the file could not be
    /// written.
    fn present(self, consent: &mut Consent) -> bool {
        let Self { what, path, edit } = self;
        let place = path.display();
        let (content, shown) = match edit {
            Edit::Done => {
                println!("{what} {place} already sets {VAR} to this.");
                return true;
            }
            Edit::Refused(reason) => {
                println!("{what} {place} is left as it is: {reason}.");
                return true;
            }
            Edit::Write { content, shown } => (content, shown),
        };
        // What is shown ends with a line break; an empty line follows, here
        // or as the next part of the output begins.
        match &shown {
            Shown::Whole => print!("{what} {place} would become:\n{content}"),
            Shown::Ending(ending) => print!("{what} {place} would end with:\n{ending}"),
        }
        if matches!(consent, Consent::Never) {
            return true;
        }
        print!("\nWrite {place}? [y/N] ");
        if !consent.agrees() {
            println!("Left as it was.");
            return true;
        }
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, content));
        match written {
            Ok(()) => {
                println!("Written.");
                true
            }
            Err(error) => {
                eprintln!("cargo-heli: cannot write {place}: {error}");
                false
            }
        }
    }
}

/// The user's home: `HOME`, or on Windows the first of the variables
/// `windows` names that holds an absolute path.
fn home(windows: &[&str]) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { windows } else { &["HOME"] };
    names.iter().find_map(|name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    })
}
