//! The systemd user unit that starts the daemon at login (Linux).
//!
//! The unit is `heli.service` in the user's unit directory,
//! `$XDG_CONFIG_HOME/systemd/user` (else `~/.config/systemd/user`),
//! enabled as `systemctl --user enable` would: the link
//! `default.target.wants/heli.service` → `../heli.service` beside it, so
//! the user manager starts it as it starts, at the user's first login.
//! setup writes both, then runs `systemctl --user daemon-reload`. It is
//! registered already when the unit holds the same definition and the link
//! is there. Removing it removes both and reloads; a daemon running now
//! keeps running. A client starts it with
//! `systemctl --user start heli.service`.
//!
//! The user manager runs from the user's first login to their last logout.
//! heli does not enable lingering, which would keep it, and the daemon,
//! running while nobody is logged in.
//!
//! The unit:
//!
//! - `ExecStart`: `cargo-heli daemon`, in the foreground. The daemon is the
//!   unit's main process; a detaching start would end the unit at once, and
//!   systemd would stop the daemon with it.
//! - `Environment`: the locations setup resolved, which the user manager's
//!   environment might not give (it lacks what a shell's profile sets).
//! - `SuccessExitStatus=3`: a daemon that finds another one running exits
//!   with 3 (`crate::daemon::ALREADY_RUNNING`), which is no failure.
//!
//! Stopping the unit stops the daemon and its runs, which are in its
//! control group.

use std::path::{Path, PathBuf};

use super::{Content, SYSTEMCTL_VAR, Step, Tool};
use crate::locations::Locations;

/// The unit's name.
const UNIT: &str = "heli.service";

/// The unit for one user's daemon.
pub struct Manager {
    unit: PathBuf,
    link: PathBuf,
    definition: Content,
}

impl Manager {
    /// The unit that starts `program` at the default `locations`.
    pub fn new(locations: &Locations, program: &str) -> Result<Self, String> {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .ok_or("neither XDG_CONFIG_HOME nor HOME is set")?;
        let dir = config_home.join("systemd").join("user");
        Ok(Self {
            unit: dir.join(UNIT),
            link: dir.join("default.target.wants").join(UNIT),
            definition: Content {
                text: definition(locations, program)?,
                utf16: false,
            },
        })
    }

    pub fn what(&self) -> String {
        format!("the systemd user unit {UNIT}")
    }

    pub fn register(&self, query: bool) -> Option<Vec<Step>> {
        let linked = std::fs::read_link(&self.link).is_ok_and(|target| target == link_target());
        if query && linked && self.definition.is_in(&self.unit) {
            return None;
        }
        Some(vec![
            Step::Write {
                path: self.unit.clone(),
                content: self.definition.clone(),
            },
            Step::Link {
                path: self.link.clone(),
                target: link_target(),
            },
            Step::Run(reload()),
        ])
    }

    pub fn unregister(&self, query: bool) -> Option<Vec<Step>> {
        let linked = !query || self.link.symlink_metadata().is_ok();
        let unit = !query || self.unit.symlink_metadata().is_ok();
        if !linked && !unit {
            return None;
        }
        let mut steps = Vec::new();
        if linked {
            steps.push(Step::Remove(self.link.clone()));
        }
        if unit {
            steps.push(Step::Remove(self.unit.clone()));
        }
        steps.push(Step::Run(reload()));
        Some(steps)
    }
}

/// The command that starts the unit.
pub fn start(_locations: &Locations) -> Option<Tool> {
    Some(Tool::new(
        SYSTEMCTL_VAR,
        "systemctl",
        ["--user", "start", UNIT],
    ))
}

fn reload() -> Tool {
    Tool::new(SYSTEMCTL_VAR, "systemctl", ["--user", "daemon-reload"])
}

/// Where the enabling link points, relative to it.
fn link_target() -> PathBuf {
    Path::new("..").join(UNIT)
}

/// The unit: `program daemon` with the `locations` setup resolved.
fn definition(locations: &Locations, program: &str) -> Result<String, String> {
    let program = quoted(program, true)?;
    let mut environment = Vec::new();
    for (name, value) in locations.vars() {
        let value = value
            .to_str()
            .ok_or_else(|| format!("{name} {} is not valid Unicode", value.display()))?;
        environment.push(quoted(&format!("{name}={value}"), false)?);
    }
    let environment = environment.join(" ");
    Ok(format!(
        "# Starts the heli daemon when you log in. Written by `cargo heli setup --autostart`;\n\
         # `cargo heli setup --no-autostart` removes it.\n\
         [Unit]\n\
         Description=heli daemon (cargo heli)\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={program} daemon\n\
         Environment={environment}\n\
         SuccessExitStatus=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

/// `text` as one quoted word of a unit file: `\` and `"` escaped, `%`
/// doubled (systemd's specifiers), and in a command line `$` doubled too
/// (its variables).
fn quoted(text: &str, command: bool) -> Result<String, String> {
    if text.contains(['\n', '\r']) {
        return Err(format!("{text:?} holds a line break"));
    }
    let mut quoted = text
        .replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    if command {
        quoted = quoted.replace('$', "$$");
    }
    Ok(format!("\"{quoted}\""))
}
