//! The Task Scheduler task that starts the daemon at logon (Windows).
//!
//! The task is `heli-daemon-<hash of the state directory>`, in the root
//! folder as other per-user tasks are (OneDrive's), so each user has one.
//! setup writes its definition to `autostart-task.xml` in the state
//! directory, UTF-16 as Task Scheduler exports tasks, and registers it with
//! `schtasks /Create /TN <task> /XML <file> /F`. It is registered already
//! when that file holds the same definition and `schtasks /Query /TN <task>`
//! finds the task. `schtasks /Delete /TN <task> /F` removes it, and a client
//! starts it with `schtasks /Run /TN <task>`.
//!
//! The definition, and the defaults it overrides:
//!
//! - a logon trigger for this user (by SID), and the principal
//!   `InteractiveToken`: the task runs only while the user is logged on, in
//!   their interactive session, and no password is stored. `LeastPrivilege`:
//!   never elevated;
//! - `Priority` 5, a normal priority class. The default, 7, is below
//!   normal with low I/O and memory priority, which every build the daemon
//!   starts would inherit;
//! - `ExecutionTimeLimit` `PT0S`: never stopped (the default stops a task
//!   after 72 h);
//! - `DisallowStartIfOnBatteries` and `StopIfGoingOnBatteries` false (both
//!   default to true): a laptop on battery still builds;
//! - `MultipleInstancesPolicy` `IgnoreNew` and `AllowStartOnDemand`, for
//!   `schtasks /Run`;
//! - the action `cargo-heli daemon --detach`. A console program that Task
//!   Scheduler starts in the user's session gets a console window; the
//!   detaching stage has it only until the daemon listens, and the daemon
//!   itself gets a console without a window, as when a client starts it
//!   (`crate::launch`).

use std::path::PathBuf;

use super::{Content, SCHTASKS_VAR, Step, Tool, xml_escape};
use crate::ipc;
use crate::locations::Locations;

/// The task for one user's daemon.
pub struct Manager {
    name: String,
    file: PathBuf,
    definition: Content,
}

impl Manager {
    /// The task that starts `program` at the default `locations`.
    pub fn new(locations: &Locations, program: &str) -> Result<Self, String> {
        let user = ipc::user_sid().map_err(|error| format!("cannot tell who you are: {error}"))?;
        Ok(Self {
            name: name(locations),
            file: locations.state_dir.join("autostart-task.xml"),
            definition: Content {
                text: definition(&user, program),
                utf16: true,
            },
        })
    }

    pub fn what(&self) -> String {
        format!("the Task Scheduler task {}", self.name)
    }

    pub fn register(&self, query: bool) -> Option<Vec<Step>> {
        if query && self.definition.is_in(&self.file) && self.query().succeeds() {
            return None;
        }
        Some(vec![
            Step::Write {
                path: self.file.clone(),
                content: self.definition.clone(),
            },
            Step::Run(Tool::new(
                SCHTASKS_VAR,
                "schtasks",
                [
                    "/Create".into(),
                    "/TN".into(),
                    self.name.clone().into(),
                    "/XML".into(),
                    self.file.clone().into_os_string(),
                    "/F".into(),
                ],
            )),
        ])
    }

    pub fn unregister(&self, query: bool) -> Option<Vec<Step>> {
        let registered = !query || self.query().succeeds();
        let file = !query || self.file.exists();
        let mut steps = Vec::new();
        if registered {
            steps.push(Step::Run(Tool::new(
                SCHTASKS_VAR,
                "schtasks",
                ["/Delete", "/TN", &self.name, "/F"],
            )));
        }
        if file {
            steps.push(Step::Remove(self.file.clone()));
        }
        (!steps.is_empty()).then_some(steps)
    }

    fn query(&self) -> Tool {
        Tool::new(SCHTASKS_VAR, "schtasks", ["/Query", "/TN", &self.name])
    }
}

/// The command that starts the task for `locations`.
pub fn start(locations: &Locations) -> Option<Tool> {
    Some(Tool::new(
        SCHTASKS_VAR,
        "schtasks",
        ["/Run", "/TN", &name(locations)],
    ))
}

fn name(locations: &Locations) -> String {
    format!("heli-daemon-{:016x}", locations.state_dir_hash())
}

/// The task definition: `program daemon --detach` at the logon of the user
/// with SID `user`, in their session.
fn definition(user: &str, program: &str) -> String {
    // Task Scheduler reads a command with a space in it whole only quoted,
    // as its own editor writes it.
    let command = if program.contains(' ') {
        format!("\"{program}\"")
    } else {
        program.to_owned()
    };
    let user = xml_escape(user);
    let command = xml_escape(&command);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the heli daemon when you log on, in your session. Written by `cargo heli setup --autostart`; `cargo heli setup --no-autostart` removes it.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>daemon --detach</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}
