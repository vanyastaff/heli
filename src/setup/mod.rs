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
//! **The build store** ([`cargo`]): then setup offers cargo's
//! `build.build-dir`, pointing at heli's build store, for this repository
//! and for every project, each file with its own question, as above.
//!
//! **Recycling** ([`hook`]): last, in a repository, setup offers its git
//! `post-checkout` hook, with which `git worktree add` gives the new
//! worktree a removed worktree's build directory. It is written executable.
//!
//! **`--autostart`** also registers the daemon to start at logon, and
//! **`--no-autostart`** removes that registration ([`autostart`]); the flag
//! is the consent. When heli cannot make the registration at all (location
//! variables are set), setup refuses before it does anything else.

mod autostart;
mod cargo;
mod claude;
mod hook;
mod profile;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::autostart::Registration;
use crate::config::Config;
use crate::locations::Locations;
use crate::{pool, store};

/// The user's Cargo home, for `cargo heli doctor`.
pub fn cargo_home(cwd: &Path) -> Option<PathBuf> {
    cargo::home(cwd)
}

/// The configuration file cargo reads in `dir`, for `cargo heli doctor`.
pub fn cargo_config_file(dir: &Path) -> PathBuf {
    cargo::file(dir)
}

/// The variable that hands a cargo the host pool.
pub const VAR: &str = "CARGO_MAKEFLAGS";

/// `cargo heli setup [--print] [--autostart | --no-autostart]`: `autostart`
/// is `Some(true)` to register the daemon at logon, `Some(false)` to remove
/// that registration.
pub fn main(print: bool, autostart: Option<bool>) -> ! {
    // Checked first, so that a registration heli cannot make changes nothing.
    let registration = autostart.map(|register| match Registration::new() {
        Ok(registration) => (register, registration),
        Err(reason) => {
            let verb = if register { "register" } else { "remove" };
            eprintln!("cargo-heli: cannot {verb} autostart: {reason}");
            std::process::exit(1)
        }
    });
    let locations = Locations::resolve().unwrap_or_else(|reason| {
        eprintln!("cargo-heli: cannot name the host pool: {reason}");
        std::process::exit(1)
    });
    let makeflags = match pool::makeflags(&locations.pool) {
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
        sets: VAR,
        edit: claude::edit(&path, &makeflags),
        path,
    });
    let profile = profile::locate()
        .map(|(path, syntax)| Offer {
            what: "Shell profile",
            sets: VAR,
            edit: profile::edit(&path, syntax, &makeflags),
            path,
        })
        .ok_or_else(|| profile::hint(&makeflags));
    let (repository, user) = build_store_offers(&locations);
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
    let mut present = |target: Result<Offer, String>| {
        println!();
        match target {
            Ok(offer) => failed |= !offer.present(&mut consent),
            Err(note) => println!("{note}"),
        }
    };
    present(claude);
    present(profile);
    println!(
        "\nWith {} set, cargo keeps a workspace's intermediates (deps, incremental, \
         fingerprints, build scripts) in heli's build store on its volume, and its target/ \
         keeps the final artifacts. Tools that look for intermediates in \
         target/<profile>/deps no longer find them there.",
        cargo::SETTING
    );
    present(repository);
    present(user);
    if let Some(hook) = hook_offer() {
        println!(
            "\nWith the repository's post-checkout hook, `git worktree add` gives the new \
             worktree the build directory of a removed worktree of the repository, renamed \
             into place, so that its first build is warm."
        );
        present(Ok(hook));
    }
    if cfg!(windows) {
        println!();
        failed |= !offer_user_environment(&mut consent, &makeflags);
    }
    if let Some((register, registration)) = &registration {
        println!();
        failed |= !if *register {
            autostart::register(registration, print)
        } else {
            autostart::unregister(registration, print)
        };
    }
    println!(
        "\nTo start the daemon as an agent session starts, run `cargo heli ensure` \
         from the harness's session-start hook."
    );
    if registration.is_none() {
        println!("To start it when you log on, run `cargo heli setup --autostart`.");
    }
    std::process::exit(i32::from(failed))
}

/// On Windows, offers to set `CARGO_MAKEFLAGS` in the user environment
/// (`crate::user_env`), which every process the user starts reads:
/// terminals, editors, rust-analyzer, Task Scheduler. False when it could
/// not be set.
fn offer_user_environment(consent: &mut Consent, makeflags: &str) -> bool {
    let place = r"Your user environment (HKCU\Environment)";
    if crate::user_env::get(VAR).is_some_and(|value| value == makeflags) {
        println!("{place} already sets {VAR} to this.");
        return true;
    }
    println!(
        "{place}, which every process you start reads (terminals, editors, rust-analyzer, \
         Task Scheduler), would set:\n{VAR}={makeflags}"
    );
    if matches!(consent, Consent::Never) {
        return true;
    }
    print!("\nSet it there? [y/N] ");
    if !consent.agrees() {
        println!("Left as it was.");
        return true;
    }
    match crate::user_env::set(VAR, std::ffi::OsStr::new(makeflags)) {
        Ok(()) => {
            println!("Set. Programs started from now on get it.");
            true
        }
        Err(error) => {
            eprintln!("cargo-heli: cannot set {VAR} in the user environment: {error}");
            false
        }
    }
}

/// The Cargo configurations that would set `build.build-dir`: this
/// repository's and the user's, or for each why there is none.
fn build_store_offers(locations: &Locations) -> (Result<Offer, String>, Result<Offer, String>) {
    let config = match Config::load(&locations.config) {
        Ok(config) => config,
        Err(reason) => {
            let note = format!("Build store: none offered ({reason}).");
            return (Err(note.clone()), Err(note));
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            let note = format!("Build store: none offered (no working directory: {error}).");
            return (Err(note.clone()), Err(note));
        }
    };
    let offer = |what: &'static str, dir: &Path, volume_of: &Path| -> Result<Offer, String> {
        let path = cargo::file(dir);
        let store = store::serving(&config.store, volume_of)
            .map_err(|reason| format!("{what}: none offered ({reason})."))?;
        let value = store::build_dir(&store);
        let edit = match value.to_str() {
            Some(value) => cargo::edit(&path, value),
            None => Edit::Refused(format!("the store {} is not Unicode", store.display())),
        };
        Ok(Offer {
            what,
            sets: cargo::SETTING,
            path,
            edit,
        })
    };
    let repository = cargo::repository(&cwd).and_then(|main| {
        offer(
            "Cargo configuration for this repository",
            &main.join(".cargo"),
            &main,
        )
    });
    let user = match cargo::home(&cwd) {
        Some(home) => offer("Cargo configuration for every project", &home, &cwd),
        None => Err("Cargo configuration for every project: none (no home directory).".to_owned()),
    };
    (repository, user)
}

/// The `post-checkout` hook of the repository setup runs in, running this
/// very program; none outside a repository.
fn hook_offer() -> Option<Offer> {
    let cwd = std::env::current_dir().ok()?;
    let path = hook::path(&cwd).ok()?;
    let edit = match std::env::current_exe() {
        Ok(program) => hook::edit(&path, &program),
        Err(error) => Edit::Refused(format!("heli cannot tell its own path ({error})")),
    };
    Some(Offer {
        what: "Git post-checkout hook for this repository",
        sets: hook::SETTING,
        path,
        edit,
    })
}

/// A file setup offers to edit.
struct Offer {
    /// What the file is, for people.
    what: &'static str,
    /// The setting it gets, as people know it.
    sets: &'static str,
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
    /// Write `content` as the file's whole content, shown whole, and make
    /// it executable: a script.
    Script { content: String },
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
        let Self {
            what,
            sets,
            path,
            edit,
        } = self;
        let place = path.display();
        let (content, shown, executable) = match edit {
            Edit::Done => {
                println!("{what} {place} already sets {sets} to this.");
                return true;
            }
            Edit::Refused(reason) => {
                println!("{what} {place} is left as it is: {reason}.");
                return true;
            }
            Edit::Write { content, shown } => (content, shown, false),
            Edit::Script { content } => (content, Shown::Whole, true),
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
            .and_then(|()| std::fs::write(&path, content))
            .and_then(|()| {
                if executable {
                    make_executable(&path)
                } else {
                    Ok(())
                }
            });
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

/// Lets everyone who may read the file at `path` run it too; on Windows
/// there is nothing to change.
fn make_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        let mode = permissions.mode();
        // Each read bit grants the execute bit beside it.
        permissions.set_mode(mode | ((mode & 0o444) >> 2));
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
