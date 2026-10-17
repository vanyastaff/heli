//! Autostart (DESIGN.md §3, §4 "Reach", §10): the registration that starts
//! the daemon when the user logs on, in the user's own session, and through
//! which clients start it afterwards.
//!
//! - **Windows**: a Task Scheduler task (`autostart/task.rs`) that runs only
//!   while the user is logged on, in the interactive session.
//! - **Linux**: a systemd user unit (`autostart/systemd.rs`), wanted by the user
//!   manager's `default.target`.
//! - **macOS**: a launchd agent (`autostart/launchd.rs`) in the user's GUI
//!   session.
//!
//! Never a service, nor a task that runs while the user is logged off: a
//! daemon there would have no desktop, and neither would its runs.
//!
//! **The service managers** are reached only through `schtasks`,
//! `systemctl` and `launchctl`, or the programs `HELI_SCHTASKS`,
//! `HELI_SYSTEMCTL` and `HELI_LAUNCHCTL` name (tests point them at a fake).
//!
//! **Default locations only.** A Task Scheduler task cannot hand its program
//! variables, so the registration starts the daemon that a plain
//! `cargo heli` finds, and setup refuses to register or unregister while
//! one of `HELI_STATE_DIR`, `HELI_ENDPOINT`, `HELI_POOL` or `HELI_CONFIG` is
//! set. The unit and the agent still name the locations setup resolved, so
//! that the service manager's environment (which may lack the XDG variables
//! a shell sets) cannot move them.
//!
//! **The record**, `autostart` in the state directory, says that setup
//! registered the daemon, with which program and for which locations. A
//! client that finds no daemon starts it through the registration
//! ([`start`]) when the record names the client's own program and
//! locations, since the registration's daemon is that program at those
//! locations. Started so, the daemon belongs to the service manager, not to
//! the client: on Windows it is outside the job of the agent that ran the
//! client. Otherwise, or when the service manager refuses, the client starts
//! the daemon itself (`crate::launch`).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::locations::{self, Locations};
use crate::nesting;
use crate::protocol::Text;

#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(windows)]
mod task;

#[cfg(target_os = "macos")]
use launchd as platform;
#[cfg(target_os = "linux")]
use systemd as platform;
#[cfg(windows)]
use task as platform;

/// Where there is no service manager heli knows.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{Locations, Step, Tool};

    pub struct Manager;

    impl Manager {
        pub fn new(_locations: &Locations, _program: &str) -> Result<Self, String> {
            Err("heli knows no service manager on this platform".to_owned())
        }

        pub fn what(&self) -> String {
            unreachable!()
        }

        pub fn register(&self, _query: bool) -> Option<Vec<Step>> {
            unreachable!()
        }

        pub fn unregister(&self, _query: bool) -> Option<Vec<Step>> {
            unreachable!()
        }
    }

    pub fn start(_locations: &Locations) -> Option<Tool> {
        None
    }
}

/// Selects the program run as `schtasks`.
#[cfg(windows)]
pub const SCHTASKS_VAR: &str = "HELI_SCHTASKS";
/// Selects the program run as `systemctl`.
#[cfg(target_os = "linux")]
pub const SYSTEMCTL_VAR: &str = "HELI_SYSTEMCTL";
/// Selects the program run as `launchctl`.
#[cfg(target_os = "macos")]
pub const LAUNCHCTL_VAR: &str = "HELI_LAUNCHCTL";

/// How long a client waits for the service manager to accept a start.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// The registration of this program for the default locations.
pub struct Registration {
    locations: Locations,
    program: PathBuf,
    manager: platform::Manager,
}

/// One step of registering or unregistering.
pub enum Step {
    /// Write `content` to the file `path`, creating its directory.
    Write { path: PathBuf, content: Content },
    /// Make `path` a symbolic link to `target`, replacing what is there.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Link { path: PathBuf, target: PathBuf },
    /// Remove the file `path`.
    Remove(PathBuf),
    /// Run a service manager command, which must succeed.
    Run(Tool),
}

/// What a [`Step::Write`] writes: text, and how it is encoded.
#[derive(Clone)]
pub struct Content {
    pub text: String,
    /// As UTF-16 with a byte order mark (else UTF-8).
    pub utf16: bool,
}

/// A service manager command.
pub struct Tool {
    /// What it is called, for people: `schtasks`.
    name: &'static str,
    program: OsString,
    args: Vec<OsString>,
}

impl Registration {
    /// The registration for this program and the default locations, or why
    /// there can be none.
    pub fn new() -> Result<Self, String> {
        if let Some(var) = locations::VARS
            .into_iter()
            .find(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
        {
            return Err(format!(
                "{var} is set, and the registration starts the daemon heli finds by default; \
                 unset it first"
            ));
        }
        let locations = Locations::resolve()?;
        let program = std::env::current_exe()
            .map_err(|error| format!("cannot tell where this cargo-heli is: {error}"))?;
        let manager = {
            let program = program
                .to_str()
                .ok_or_else(|| format!("the path {} is not valid Unicode", program.display()))?;
            platform::Manager::new(&locations, program)?
        };
        Ok(Self {
            locations,
            program,
            manager,
        })
    }

    /// What the registration is, for people: "the Task Scheduler task …".
    pub fn what(&self) -> String {
        self.manager.what()
    }

    /// The steps that register the daemon, or `None` when it is registered
    /// already, as the service manager says (with `query`; without, the
    /// steps of a first registration).
    pub fn register_steps(&self, query: bool) -> Option<Vec<Step>> {
        self.manager.register(query)
    }

    /// The steps that remove the registration, or `None` when there is none
    /// (with `query`; without, the steps that remove one).
    pub fn unregister_steps(&self, query: bool) -> Option<Vec<Step>> {
        self.manager.unregister(query)
    }

    /// Writes the record that lets clients start the daemon through the
    /// registration.
    pub fn write_record(&self) -> io::Result<()> {
        let record = Record::new(&self.program, &self.locations);
        let json = serde_json::to_string(&record).expect("BUG: the record serializes");
        self.locations.create_state_dir()?;
        std::fs::write(record_path(&self.locations), json)
    }

    /// Removes the record, so that clients start the daemon themselves.
    pub fn remove_record(&self) -> io::Result<()> {
        match std::fs::remove_file(record_path(&self.locations)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

impl Step {
    /// Takes the step.
    pub fn take(&self) -> Result<(), String> {
        match self {
            Self::Write { path, content } => path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(path, content.bytes()))
                .map_err(|error| format!("cannot write {}: {error}", path.display())),
            Self::Link { path, target } => link(path, target)
                .map_err(|error| format!("cannot link {}: {error}", path.display())),
            Self::Remove(path) => match std::fs::remove_file(path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    Err(format!("cannot remove {}: {error}", path.display()))
                }
                _ => Ok(()),
            },
            Self::Run(tool) => tool.run(),
        }
    }
}

impl Content {
    fn bytes(&self) -> Vec<u8> {
        if self.utf16 {
            [0xfeff_u16]
                .into_iter()
                .chain(self.text.encode_utf16())
                .flat_map(u16::to_le_bytes)
                .collect()
        } else {
            self.text.as_bytes().to_vec()
        }
    }

    /// Whether the file `path` holds exactly this.
    fn is_in(&self, path: &Path) -> bool {
        std::fs::read(path).is_ok_and(|bytes| bytes == self.bytes())
    }
}

impl Tool {
    /// `name` with `args`, run as the program `var` names, else as `name`.
    fn new<I, S>(var: &str, name: &'static str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            name,
            program: std::env::var_os(var)
                .filter(|program| !program.is_empty())
                .unwrap_or_else(|| name.into()),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// Runs it with no input and its output captured: `Ok` when it
    /// succeeds, else what it said.
    fn run(&self) -> Result<(), String> {
        let output = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run `{self}`: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let said = [&output.stderr, &output.stdout]
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
            .find(|text| !text.is_empty())
            .map(|text| format!(": {}", text.replace(['\r', '\n'], " ")))
            .unwrap_or_default();
        Err(format!("`{self}` failed ({}){said}", output.status))
    }

    /// Whether it succeeds, its output discarded: for a question.
    #[cfg(any(windows, target_os = "macos"))]
    fn succeeds(&self) -> bool {
        Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Runs it for a client at nesting depth `depth`, with no stdio (it
    /// must not write into the client's output), for up to
    /// [`START_TIMEOUT`]: true when it succeeded.
    fn start(&self, depth: u32) -> bool {
        let child = Command::new(&self.program)
            .args(&self.args)
            .env(nesting::VAR, (depth + 1).to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return false;
        };
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)?;
        for arg in &self.args {
            let arg = arg.to_string_lossy();
            if arg.contains(' ') {
                write!(f, " \"{arg}\"")?;
            } else {
                write!(f, " {arg}")?;
            }
        }
        Ok(())
    }
}

/// Asks the registration to start the daemon for `locations`, when setup
/// registered one for this program and these locations; `depth` is this
/// client's. True when the service manager accepted: the daemon then comes
/// up on its own, not as this process's child.
pub fn start(locations: &Locations, depth: u32) -> bool {
    let Ok(program) = std::env::current_exe() else {
        return false;
    };
    let recorded = std::fs::read(record_path(locations))
        .ok()
        .and_then(|json| serde_json::from_slice::<Record>(&json).ok());
    if recorded != Some(Record::new(&program, locations)) {
        return false;
    }
    platform::start(locations).is_some_and(|tool| tool.start(depth))
}

/// What setup registered: the daemon's program, and its locations.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    program: Text,
    state_dir: Text,
    endpoint: Text,
    pool: Text,
    config: Text,
}

impl Record {
    fn new(program: &Path, locations: &Locations) -> Self {
        Self {
            program: Text(program.into()),
            state_dir: Text(locations.state_dir.clone().into()),
            endpoint: Text(locations.endpoint.clone().into()),
            pool: Text(locations.pool.clone()),
            config: Text(locations.config.clone().into()),
        }
    }
}

fn record_path(locations: &Locations) -> PathBuf {
    locations.state_dir.join("autostart")
}

#[cfg(unix)]
fn link(path: &Path, target: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn link(_path: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no symbolic links here",
    ))
}

/// `text` as the content of an XML element: the characters markup gives a
/// meaning there escaped.
#[cfg(any(windows, target_os = "macos"))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
