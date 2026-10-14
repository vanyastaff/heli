//! The launchd agent that starts the daemon at login (macOS).
//!
//! The agent is `io.github.vanyastaff.heli`, defined by
//! `~/Library/LaunchAgents/io.github.vanyastaff.heli.plist`, which launchd
//! loads into the user's GUI domain, `gui/<uid>`, at every login. setup
//! writes the file and loads it now, with
//! `launchctl bootstrap gui/<uid> <file>`, which also starts the daemon
//! (`RunAtLoad`). It is registered already when the file holds the same
//! definition and `launchctl print gui/<uid>/<label>` finds the agent
//! loaded. An agent loaded with another definition is unloaded first
//! (`launchctl bootout gui/<uid>/<label>`, which stops a daemon launchd
//! started), then loaded again. Removing it unloads it and removes the
//! file. A client starts it with `launchctl kickstart gui/<uid>/<label>`.
//!
//! The definition:
//!
//! - `ProgramArguments`: `cargo-heli daemon`, in the foreground;
//! - `EnvironmentVariables`: the locations setup resolved, which launchd's
//!   environment does not give (it lacks what a shell's profile sets);
//! - `RunAtLoad`: started as it is loaded, at login;
//! - `LimitLoadToSessionType` `Aqua`: only in a GUI login session;
//! - `ProcessType` `Interactive`: the resource limits of an app, none.
//!   Without it, launchd throttles the CPU and I/O of an agent, and of every
//!   build the daemon starts;
//! - `AbandonProcessGroup`: launchd leaves the runs alone when the daemon
//!   exits;
//! - no `KeepAlive`: a daemon that exits stays down until a client starts it
//!   again, and one that finds another daemon running exits at once.

use std::path::PathBuf;

use super::{Content, LAUNCHCTL_VAR, Step, Tool, xml_escape};
use crate::locations::Locations;

/// The agent's label.
const LABEL: &str = "io.github.vanyastaff.heli";

/// The agent for one user's daemon.
pub struct Manager {
    plist: PathBuf,
    definition: Content,
}

impl Manager {
    /// The agent that starts `program` at the default `locations`.
    pub fn new(locations: &Locations, program: &str) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or("HOME is not set")?;
        Ok(Self {
            plist: PathBuf::from(home)
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LABEL}.plist")),
            definition: Content {
                text: definition(locations, program)?,
                utf16: false,
            },
        })
    }

    pub fn what(&self) -> String {
        format!("the launchd agent {LABEL}")
    }

    pub fn register(&self, query: bool) -> Option<Vec<Step>> {
        let loaded = query && print().succeeds();
        if loaded && self.definition.is_in(&self.plist) {
            return None;
        }
        let mut steps = Vec::new();
        if loaded {
            steps.push(Step::Run(bootout()));
        }
        steps.push(Step::Write {
            path: self.plist.clone(),
            content: self.definition.clone(),
        });
        steps.push(Step::Run(Tool::new(
            LAUNCHCTL_VAR,
            "launchctl",
            [
                "bootstrap".into(),
                domain().into(),
                self.plist.clone().into_os_string(),
            ],
        )));
        Some(steps)
    }

    pub fn unregister(&self, query: bool) -> Option<Vec<Step>> {
        let loaded = !query || print().succeeds();
        let file = !query || self.plist.exists();
        let mut steps = Vec::new();
        if loaded {
            steps.push(Step::Run(bootout()));
        }
        if file {
            steps.push(Step::Remove(self.plist.clone()));
        }
        (!steps.is_empty()).then_some(steps)
    }
}

/// The command that starts the agent.
pub fn start(_locations: &Locations) -> Option<Tool> {
    Some(Tool::new(
        LAUNCHCTL_VAR,
        "launchctl",
        ["kickstart", &service()],
    ))
}

fn print() -> Tool {
    Tool::new(LAUNCHCTL_VAR, "launchctl", ["print", &service()])
}

fn bootout() -> Tool {
    Tool::new(LAUNCHCTL_VAR, "launchctl", ["bootout", &service()])
}

/// The user's GUI domain, `gui/<uid>`.
fn domain() -> String {
    format!("gui/{}", rustix::process::getuid().as_raw())
}

/// The agent in that domain, `gui/<uid>/<label>`.
fn service() -> String {
    format!("{}/{LABEL}", domain())
}

/// The property list: `program daemon` with the `locations` setup resolved.
fn definition(locations: &Locations, program: &str) -> Result<String, String> {
    let mut environment = String::new();
    for (name, value) in locations.vars() {
        let value = value
            .to_str()
            .ok_or_else(|| format!("{name} {} is not valid Unicode", value.display()))?;
        environment.push_str(&format!(
            "    <key>{name}</key>\n    <string>{}</string>\n",
            xml_escape(value)
        ));
    }
    let program = xml_escape(program);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>daemon</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{environment}  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>AbandonProcessGroup</key>
  <true/>
</dict>
</plist>
"#
    ))
}
