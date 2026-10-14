//! The fake service manager (`tests/fixtures/basic/crates/fake-manager`),
//! which autostart tests put in place of `schtasks`, `systemctl` and
//! `launchctl`: it records each call, keeps one registration in a file of
//! the fixture's, and on a start call starts what a test tells it to.
//!
//! Every command a fixture makes already points heli's service-manager
//! variables at a program that does not exist ([`super::Fixture::command`]),
//! so no test reaches the real tools; [`FakeManager::apply`] points them at
//! the fake instead.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::Fixture;

/// The variables through which heli reaches the service managers.
pub const VARS: [&str; 3] = ["HELI_SCHTASKS", "HELI_SYSTEMCTL", "HELI_LAUNCHCTL"];

/// The fake, with its log and registration in a fixture's temporary
/// directory.
pub struct FakeManager {
    program: PathBuf,
    log: PathBuf,
    state: PathBuf,
}

impl FakeManager {
    /// The fake built by `builder` (a fixture with the real home, where
    /// cargo lives), keeping its log and registration in `fixture`'s
    /// temporary directory.
    pub fn new(builder: &Fixture, fixture: &Fixture) -> Self {
        Self {
            program: builder.binary("fake-manager"),
            log: fixture.path("fake-manager.log"),
            state: fixture.path("fake-manager.registered"),
        }
    }

    /// Makes `command` reach this fake for every service manager.
    pub fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        for var in VARS {
            command.env(var, &self.program);
        }
        command
            .env("FAKE_MANAGER_LOG", &self.log)
            .env("FAKE_MANAGER_STATE", &self.state)
    }

    /// Makes `command` reach this fake, which starts `program` with `args`
    /// (separated by spaces) when asked to start the registration.
    pub fn apply_starting<'a>(
        &self,
        command: &'a mut Command,
        program: impl AsRef<OsStr>,
        args: &str,
    ) -> &'a mut Command {
        self.apply(command)
            .env("FAKE_MANAGER_START", program.as_ref())
            .env("FAKE_MANAGER_START_ARGS", args)
    }

    /// Every call so far, each as its arguments.
    pub fn calls(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .map(|line| {
                if line.is_empty() {
                    Vec::new()
                } else {
                    line.split('\t').map(str::to_owned).collect()
                }
            })
            .collect()
    }

    /// Forgets the calls so far.
    pub fn clear_calls(&self) {
        let _ = std::fs::remove_file(&self.log);
    }

    /// Whether the fake holds a registration.
    pub fn registered(&self) -> bool {
        self.state.exists()
    }

    /// The fake's program.
    pub fn program(&self) -> &Path {
        &self.program
    }
}
