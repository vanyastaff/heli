//! `cargo heli setup --autostart` and `--no-autostart` (DESIGN.md §3, §4
//! "Reach", §10): the daemon registered to start at logon, in the user's
//! session, and clients that start it through that registration.
//!
//! Every test runs as a user with a temporary home ([`Fixture::with_home`]),
//! so heli's default locations are in that home, and with the fake service
//! manager ([`FakeManager`]) in place of `schtasks`, `systemctl` and
//! `launchctl`: no test registers a real task, unit or agent.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use support::fake_manager::FakeManager;
use support::{Fixture, assert_success, heli_binary};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A user whose home is a fixture's temporary home, and the fake service
/// manager, built by a second fixture (cargo needs the real home).
struct User {
    fixture: Fixture,
    manager: FakeManager,
    builder: Fixture,
}

impl User {
    fn new() -> Self {
        let builder = Fixture::new("basic");
        let fixture = Fixture::with_home("basic", false);
        let manager = FakeManager::new(&builder, &fixture);
        Self {
            fixture,
            manager,
            builder,
        }
    }

    /// A user with the daemon registered at logon; the fake's calls so far
    /// are cleared. Returns the registration's names.
    fn registered() -> (Self, platform::Names) {
        let user = Self::new();
        let output = user.setup(&["--autostart"]);
        assert_success(&output, "cargo heli setup --autostart");
        let names = platform::Names::from(&user.manager.calls());
        user.manager.clear_calls();
        (user, names)
    }

    fn home(&self) -> PathBuf {
        self.fixture.home()
    }

    /// `cargo heli setup <args>` with no input, reaching the fake.
    fn setup_command(&self, args: &[&str]) -> Command {
        let mut command = self.fixture.setup(None, args.iter().copied());
        self.manager.apply(&mut command);
        command.stdin(Stdio::null());
        command
    }

    /// Runs `cargo heli setup <args>` with no input, reaching the fake.
    fn setup(&self, args: &[&str]) -> Output {
        self.setup_command(args).output().expect("run heli setup")
    }

    /// `cargo heli <args>` as this user, reaching the fake.
    fn heli(&self, args: &[&str]) -> Command {
        let mut command = self.fixture.heli(args.iter().copied());
        self.manager.apply(&mut command);
        command
    }
}

/// The line `setup` prints when it has registered the daemon.
const REGISTERED: &str = "Registered.";

/// `command` as it is, but running `program`.
fn with_program(command: &Command, program: &Path) -> Command {
    let mut other = Command::new(program);
    other.args(command.get_args());
    if let Some(dir) = command.get_current_dir() {
        other.current_dir(dir);
    }
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => other.env(name, value),
            None => other.env_remove(name),
        };
    }
    other
}

/// A copy of heli at `name` in `fixture`'s temporary directory: another
/// program, as an install elsewhere would be.
fn heli_copy(fixture: &Fixture, name: &str) -> PathBuf {
    let dir = fixture.path(name);
    std::fs::create_dir_all(&dir).expect("create the copy's directory");
    let copy = dir.join(heli_binary().file_name().expect("a file name"));
    std::fs::copy(heli_binary(), &copy).expect("copy cargo-heli");
    copy
}

/// What `stdout` shows under the line `header`: a definition's whole text,
/// up to the empty line that ends it, before the next step or the line that
/// closes setup's account. The text may hold empty lines of its own (a
/// systemd unit's, between its sections).
fn shown(stdout: &str, header: &str) -> String {
    const NEXT: [&str; 6] = [
        "Definition ",
        "Link ",
        "Remove ",
        "Run ",
        "Nothing ",
        "Registered.",
    ];
    let mut lines = stdout.lines();
    assert!(
        lines.any(|line| line == header),
        "no line {header:?} in:\n{stdout}"
    );
    let shown: String = lines
        .take_while(|line| !NEXT.iter().any(|next| line.starts_with(next)))
        .map(|line| format!("{line}\n"))
        .collect();
    shown
        .strip_suffix("\n\n")
        .map(|text| format!("{text}\n"))
        .unwrap_or_else(|| panic!("no empty line ends the text under {header:?} in:\n{stdout}"))
}

/// What autostart registers on Windows: the Task Scheduler task
/// `heli-daemon-<hash of the state directory>`, defined by
/// `autostart-task.xml` in the state directory.
#[cfg(windows)]
mod platform {
    use std::path::{Path, PathBuf};

    use super::FakeManager;

    /// The registration as calls name it: the task.
    pub struct Names {
        task: String,
    }

    impl Names {
        /// The names in `calls`, those of a first registration.
        pub fn from(calls: &[Vec<String>]) -> Self {
            let task = calls
                .first()
                .and_then(|call| call.get(2))
                .unwrap_or_else(|| panic!("no task named in {calls:?}"))
                .clone();
            let hash = task.strip_prefix("heli-daemon-").unwrap_or_default();
            assert!(
                hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()),
                "the task's name: {task}"
            );
            Self { task }
        }

        fn query(&self) -> Vec<String> {
            strings(&["/Query", "/TN", &self.task])
        }

        /// The calls of a first registration: with no definition yet,
        /// there is nothing to ask about.
        pub fn fresh(&self, home: &Path) -> Vec<Vec<String>> {
            vec![self.create(home)]
        }

        /// The calls of a registration that is in place already: a
        /// question.
        pub fn unchanged(&self, _home: &Path) -> Vec<Vec<String>> {
            vec![self.query()]
        }

        /// The calls of a registration that replaces one with another
        /// definition.
        pub fn replaced(&self, home: &Path) -> Vec<Vec<String>> {
            vec![self.create(home)]
        }

        /// The calls that remove the registration.
        pub fn removed(&self) -> Vec<Vec<String>> {
            vec![self.query(), strings(&["/Delete", "/TN", &self.task, "/F"])]
        }

        /// The calls of a removal when nothing is registered: a question.
        pub fn absent(&self) -> Vec<Vec<String>> {
            vec![self.query()]
        }

        /// The call with which a client starts the registration.
        pub fn start(&self) -> Vec<String> {
            strings(&["/Run", "/TN", &self.task])
        }

        fn create(&self, home: &Path) -> Vec<String> {
            let definition = definition(home);
            let definition = definition.to_str().expect("a Unicode path");
            strings(&["/Create", "/TN", &self.task, "/XML", definition, "/F"])
        }
    }

    /// The service manager's verb that makes the registration.
    pub const REGISTER_VERB: &str = "/Create";
    /// The verb that starts it.
    pub const START_VERB: &str = "/Run";
    /// What the registration runs: heli's arguments.
    pub const START_ARGS: &str = "daemon --detach";
    /// The depth of a client from which a start through the registration
    /// brings up no daemon: the service manager runs `daemon --detach` one
    /// level deeper, and the daemon is one more, at the limit of 16.
    pub const TOO_DEEP: &str = "14";

    /// The file that holds the registration's definition.
    pub fn definition(home: &Path) -> PathBuf {
        home.join("heli").join("autostart-task.xml")
    }

    /// The definition's text: UTF-16 with a byte order mark, as Task
    /// Scheduler exports a task.
    pub fn read(path: &Path) -> String {
        let bytes =
            std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let body = bytes
            .strip_prefix(&[0xff, 0xfe])
            .expect("a UTF-16LE byte order mark");
        let (pairs, rest) = body.as_chunks::<2>();
        assert!(rest.is_empty(), "an odd number of bytes");
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        String::from_utf16(&units).expect("UTF-16")
    }

    /// The definition of the task that starts `program` at the logon of
    /// the user running the tests: in their session, only while they are
    /// logged on, never stopped, on battery too, at normal priority. A
    /// program whose path has a space is quoted, as Task Scheduler's own
    /// editor writes it, and an `&` in it is XML's `&amp;`.
    pub fn expected(program: &Path, _home: &Path) -> String {
        let sid = user_sid();
        let program = program.display().to_string().replace('&', "&amp;");
        let program = if program.contains(' ') {
            format!("\"{program}\"")
        } else {
            program
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the heli daemon when you log on, in your session. Written by `cargo heli setup --autostart`; `cargo heli setup --no-autostart` removes it.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
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
      <Command>{program}</Command>
      <Arguments>daemon --detach</Arguments>
    </Exec>
  </Actions>
</Task>
"#
        )
    }

    /// Checks the definition at `path` with Task Scheduler's own parser: a
    /// task definition built in memory from it (`ITaskService::NewTask`),
    /// which is never registered. Task Scheduler reads it as a logon task
    /// in the interactive session, not elevated, at normal priority, never
    /// stopped, on battery too, one instance, startable on demand.
    pub fn check(path: &Path, _home: &Path) {
        let script = "$ErrorActionPreference = 'Stop'
            $service = New-Object -ComObject Schedule.Service
            $service.Connect()
            $task = $service.NewTask(0)
            $task.XmlText = [IO.File]::ReadAllText($env:HELI_TEST_DEFINITION)
            $settings = $task.Settings
            'trigger ' + $task.Triggers.Item(1).Type
            'logon ' + $task.Principal.LogonType
            'run level ' + $task.Principal.RunLevel
            'priority ' + $settings.Priority
            'time limit ' + $settings.ExecutionTimeLimit
            'on battery ' + $settings.DisallowStartIfOnBatteries + ' ' + $settings.StopIfGoingOnBatteries
            'instances ' + $settings.MultipleInstances
            'on demand ' + $settings.AllowDemandStart";
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("HELI_TEST_DEFINITION", path)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run powershell");
        super::assert_success(&output, "Task Scheduler's check of the definition");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            // TASK_TRIGGER_LOGON, TASK_LOGON_INTERACTIVE_TOKEN,
            // TASK_RUNLEVEL_LUA, TASK_INSTANCES_IGNORE_NEW.
            "trigger 9\nlogon 3\nrun level 0\npriority 5\ntime limit PT0S\n\
             on battery False False\ninstances 2\non demand True\n"
        );
    }

    /// Whether the daemon is registered: the fake holds the task.
    pub fn registered(manager: &FakeManager, _home: &Path) -> bool {
        manager.registered()
    }

    /// The SID of the user running the tests, as `whoami` gives it.
    fn user_sid() -> String {
        let output = std::process::Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .expect("run whoami");
        let line = String::from_utf8_lossy(&output.stdout);
        line.trim()
            .rsplit(',')
            .next()
            .map(|sid| sid.trim_matches('"').to_owned())
            .filter(|sid| sid.starts_with("S-1-"))
            .unwrap_or_else(|| panic!("no SID from whoami: {line}"))
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }
}

/// What autostart registers on Linux: the systemd user unit `heli.service`
/// in `~/.config/systemd/user`, enabled by a link in
/// `default.target.wants`.
#[cfg(target_os = "linux")]
mod platform {
    use std::path::{Path, PathBuf};

    use super::FakeManager;

    pub const REGISTER_VERB: &str = "daemon-reload";
    pub const START_VERB: &str = "start";
    pub const START_ARGS: &str = "daemon";
    /// The service manager runs the daemon one level deeper than the
    /// client, at the limit of 16.
    pub const TOO_DEEP: &str = "15";

    /// The registration as calls name it: always `heli.service`.
    pub struct Names;

    impl Names {
        pub fn from(_calls: &[Vec<String>]) -> Self {
            Self
        }

        /// The calls of a first registration: the unit and its link are
        /// files; the user manager reloads them.
        pub fn fresh(&self, _home: &Path) -> Vec<Vec<String>> {
            vec![reload()]
        }

        /// The calls of a registration that is in place already: none.
        pub fn unchanged(&self, _home: &Path) -> Vec<Vec<String>> {
            Vec::new()
        }

        pub fn replaced(&self, _home: &Path) -> Vec<Vec<String>> {
            vec![reload()]
        }

        pub fn removed(&self) -> Vec<Vec<String>> {
            vec![reload()]
        }

        pub fn absent(&self) -> Vec<Vec<String>> {
            Vec::new()
        }

        pub fn start(&self) -> Vec<String> {
            strings(&["--user", "start", "heli.service"])
        }
    }

    fn reload() -> Vec<String> {
        strings(&["--user", "daemon-reload"])
    }

    fn units(home: &Path) -> PathBuf {
        home.join(".config").join("systemd").join("user")
    }

    fn link(home: &Path) -> PathBuf {
        units(home)
            .join("default.target.wants")
            .join("heli.service")
    }

    pub fn definition(home: &Path) -> PathBuf {
        units(home).join("heli.service")
    }

    pub fn read(path: &Path) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    /// The unit that starts `program` (as Linux names the running program:
    /// links resolved) in the foreground, with heli's default locations in
    /// `home`.
    pub fn expected(program: &Path, home: &Path) -> String {
        let program = std::fs::canonicalize(program).expect("the program's path");
        let state = home.join(".local").join("state").join("heli");
        format!(
            "# Starts the heli daemon when you log in. Written by `cargo heli setup --autostart`;\n\
             # `cargo heli setup --no-autostart` removes it.\n\
             [Unit]\n\
             Description=heli daemon (cargo heli)\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart=\"{}\" daemon\n\
             Environment=\"HELI_STATE_DIR={}\" \"HELI_ENDPOINT={}\" \"HELI_POOL={}\" \"HELI_CONFIG={}\"\n\
             SuccessExitStatus=3\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            program.display(),
            state.display(),
            state.join("daemon.sock").display(),
            home.join(".cache").join("heli").join("jobserver").display(),
            home.join(".config")
                .join("heli")
                .join("config.toml")
                .display(),
        )
    }

    /// Checks the unit with `systemd-analyze --user verify`, which loads it
    /// without installing anything, in `home`. Where systemd is not
    /// installed, there is nothing to check with.
    pub fn check(path: &Path, home: &Path) {
        use std::os::unix::fs::DirBuilderExt;

        let runtime = home.join("runtime");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .expect("create a runtime directory");
        let output = std::process::Command::new("systemd-analyze")
            .args(["--user", "verify"])
            .arg(path)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("systemd-analyze is not installed: the unit is not checked");
            }
            output => super::assert_success(
                &output.expect("run systemd-analyze"),
                "systemd-analyze --user verify",
            ),
        }
    }

    /// Whether the daemon is registered: the unit is there, and enabled.
    pub fn registered(_manager: &FakeManager, home: &Path) -> bool {
        let linked = std::fs::read_link(link(home))
            .is_ok_and(|target| target == Path::new("..").join("heli.service"));
        assert_eq!(
            linked,
            definition(home).is_file(),
            "the unit and its link go together"
        );
        linked
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }
}

/// What autostart registers on macOS: the launchd agent
/// `io.github.vanyastaff.heli`, defined in `~/Library/LaunchAgents`.
#[cfg(target_os = "macos")]
mod platform {
    use std::path::{Path, PathBuf};

    use super::FakeManager;

    pub const REGISTER_VERB: &str = "bootstrap";
    pub const START_VERB: &str = "kickstart";
    pub const START_ARGS: &str = "daemon";
    /// The service manager runs the daemon one level deeper than the
    /// client, at the limit of 16.
    pub const TOO_DEEP: &str = "15";

    const LABEL: &str = "io.github.vanyastaff.heli";

    /// The registration as calls name it: the agent in the user's GUI
    /// domain.
    pub struct Names {
        uid: String,
    }

    impl Names {
        pub fn from(_calls: &[Vec<String>]) -> Self {
            let output = std::process::Command::new("id")
                .arg("-u")
                .output()
                .expect("run id -u");
            Self {
                uid: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            }
        }

        fn service(&self) -> String {
            format!("gui/{}/{LABEL}", self.uid)
        }

        fn print(&self) -> Vec<String> {
            strings(&["print", &self.service()])
        }

        fn bootout(&self) -> Vec<String> {
            strings(&["bootout", &self.service()])
        }

        fn bootstrap(&self, home: &Path) -> Vec<String> {
            let plist = definition(home);
            strings(&[
                "bootstrap",
                &format!("gui/{}", self.uid),
                plist.to_str().expect("a Unicode path"),
            ])
        }

        /// The calls of a first registration: is it loaded? Then load it.
        pub fn fresh(&self, home: &Path) -> Vec<Vec<String>> {
            vec![self.print(), self.bootstrap(home)]
        }

        pub fn unchanged(&self, _home: &Path) -> Vec<Vec<String>> {
            vec![self.print()]
        }

        /// Loaded with another definition: unloaded, then loaded again.
        pub fn replaced(&self, home: &Path) -> Vec<Vec<String>> {
            vec![self.print(), self.bootout(), self.bootstrap(home)]
        }

        pub fn removed(&self) -> Vec<Vec<String>> {
            vec![self.print(), self.bootout()]
        }

        pub fn absent(&self) -> Vec<Vec<String>> {
            vec![self.print()]
        }

        pub fn start(&self) -> Vec<String> {
            strings(&["kickstart", &self.service()])
        }
    }

    pub fn definition(home: &Path) -> PathBuf {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    pub fn read(path: &Path) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    /// The agent that starts `program` in the foreground at login, in the
    /// GUI session, without resource limits, with heli's default locations
    /// in `home`. An `&` in the program's path is XML's `&amp;`.
    pub fn expected(program: &Path, home: &Path) -> String {
        let program = program.display().to_string().replace('&', "&amp;");
        let state = home.join(".local").join("state").join("heli");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>daemon</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HELI_STATE_DIR</key>
    <string>{}</string>
    <key>HELI_ENDPOINT</key>
    <string>{}</string>
    <key>HELI_POOL</key>
    <string>{}</string>
    <key>HELI_CONFIG</key>
    <string>{}</string>
  </dict>
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
"#,
            program,
            state.display(),
            state.join("daemon.sock").display(),
            home.join(".cache").join("heli").join("jobserver").display(),
            home.join(".config")
                .join("heli")
                .join("config.toml")
                .display(),
        )
    }

    /// Checks the property list with `plutil -lint`.
    pub fn check(path: &Path, _home: &Path) {
        let output = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run plutil");
        super::assert_success(&output, "plutil -lint");
    }

    /// Whether the daemon is registered: the fake has the agent loaded.
    pub fn registered(manager: &FakeManager, _home: &Path) -> bool {
        manager.registered()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }
}

/// `setup --autostart` registers the daemon to start at logon: it writes the
/// definition, hands it to the service manager, and says so.
#[test]
fn setup_registers_the_daemon_at_logon() {
    let user = User::new();
    let output = user.setup(&["--autostart"]);
    assert_success(&output, "cargo heli setup --autostart");
    let stdout = text(&output.stdout);
    assert!(stdout.lines().any(|line| line == REGISTERED), "{stdout}");

    let calls = user.manager.calls();
    let names = platform::Names::from(&calls);
    assert_eq!(calls, names.fresh(&user.home()));
    assert!(platform::registered(&user.manager, &user.home()));
    let definition = platform::definition(&user.home());
    assert_eq!(
        platform::read(&definition),
        platform::expected(&heli_binary(), &user.home())
    );
}

/// Registering again changes nothing: setup asks the service manager, finds
/// the registration in place as it would make it, says so, and registers
/// nothing.
#[test]
fn setup_registers_once() {
    let user = User::new();
    assert_success(&user.setup(&["--autostart"]), "setup --autostart");
    let names = platform::Names::from(&user.manager.calls());
    let definition = std::fs::read(platform::definition(&user.home())).expect("the definition");
    user.manager.clear_calls();

    for _ in 0..2 {
        let output = user.setup(&["--autostart"]);
        assert_success(&output, "setup --autostart, again");
        let stdout = text(&output.stdout);
        assert!(
            stdout
                .lines()
                .any(|line| line == "It is registered already."),
            "{stdout}"
        );
        assert!(!stdout.contains(REGISTERED), "{stdout}");
        assert_eq!(user.manager.calls(), names.unchanged(&user.home()));
        user.manager.clear_calls();
    }
    assert_eq!(
        std::fs::read(platform::definition(&user.home())).expect("the definition"),
        definition
    );
    assert!(platform::registered(&user.manager, &user.home()));
}

/// After heli's state directory is removed, registering again makes
/// clients start the daemon through the registration again.
#[test]
fn setup_again_after_the_state_directory_is_removed() {
    let (user, names) = User::registered();
    std::fs::remove_dir_all(
        user.home()
            .join(if cfg!(windows) { "heli" } else { ".local" }),
    )
    .expect("remove the state directory");
    assert_success(&user.setup(&["--autostart"]), "setup --autostart, again");
    user.manager.clear_calls();

    let mut ensure = user.heli(&["ensure"]);
    user.manager
        .apply_starting(&mut ensure, heli_binary(), platform::START_ARGS);
    let output = ensure.output().expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    assert_eq!(user.manager.calls(), vec![names.start()]);
}

/// `--no-autostart` removes the registration and its definition; run again,
/// it finds nothing to remove and changes nothing.
#[test]
fn setup_no_autostart_removes_the_registration() {
    let user = User::new();
    assert_success(&user.setup(&["--autostart"]), "setup --autostart");
    let names = platform::Names::from(&user.manager.calls());
    user.manager.clear_calls();

    let output = user.setup(&["--no-autostart"]);
    assert_success(&output, "setup --no-autostart");
    let stdout = text(&output.stdout);
    assert!(stdout.lines().any(|line| line == "Removed."), "{stdout}");
    assert_eq!(user.manager.calls(), names.removed());
    assert!(!platform::registered(&user.manager, &user.home()));
    assert!(!platform::definition(&user.home()).exists());
    user.manager.clear_calls();

    let output = user.setup(&["--no-autostart"]);
    assert_success(&output, "setup --no-autostart, again");
    let stdout = text(&output.stdout);
    assert!(stdout.contains(" is not registered."), "{stdout}");
    assert!(!stdout.contains("Removed."), "{stdout}");
    assert_eq!(user.manager.calls(), names.absent());
    user.manager.clear_calls();

    // Clients start the daemon themselves again.
    let output = user.heli(&["ensure"]).output().expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    assert_eq!(user.manager.calls(), Vec::<Vec<String>>::new());
    assert!(user.fixture.daemon_pid().is_some(), "a daemon runs");
}

/// A registration made for another program (heli installed elsewhere, here
/// in a directory whose name has a space and an `&`, which the definition
/// must carry as they are) is replaced by one for this program.
#[test]
fn setup_replaces_a_registration_of_another_program() {
    let user = User::new();
    let elsewhere = heli_copy(&user.fixture, "else & where");
    let output = with_program(&user.setup_command(&["--autostart"]), &elsewhere)
        .output()
        .expect("run the other heli's setup");
    assert_success(&output, "the other heli's setup --autostart");
    assert_eq!(
        platform::read(&platform::definition(&user.home())),
        platform::expected(&elsewhere, &user.home())
    );
    let names = platform::Names::from(&user.manager.calls());
    user.manager.clear_calls();

    let output = user.setup(&["--autostart"]);
    assert_success(&output, "setup --autostart");
    let stdout = text(&output.stdout);
    assert!(stdout.lines().any(|line| line == REGISTERED), "{stdout}");
    assert_eq!(user.manager.calls(), names.replaced(&user.home()));
    assert_eq!(
        platform::read(&platform::definition(&user.home())),
        platform::expected(&heli_binary(), &user.home())
    );
}

/// Nothing is registered without `--autostart`: plain `setup` does not
/// reach the service manager, and says how to register. `--print` shows
/// the definition it would register and the commands it would run, and
/// does neither; nor does `--print --no-autostart`.
#[test]
fn nothing_is_registered_without_the_flag() {
    let user = User::new();
    let output = user.setup(&[]);
    assert_success(&output, "setup");
    let stdout = text(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line == "To start it when you log on, run `cargo heli setup --autostart`."),
        "{stdout}"
    );
    assert!(!stdout.contains("Autostart"), "{stdout}");

    let output = user.setup(&["--print", "--autostart"]);
    assert_success(&output, "setup --print --autostart");
    let stdout = text(&output.stdout);
    let definition = platform::definition(&user.home());
    let header = format!("Definition {}:", definition.display());
    assert_eq!(
        shown(&stdout, &header),
        platform::expected(&heli_binary(), &user.home())
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == "Nothing registered: --print changes nothing."),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("Run ") && line.contains(platform::REGISTER_VERB)),
        "the command it would run: {stdout}"
    );

    let output = user.setup(&["--print", "--no-autostart"]);
    assert_success(&output, "setup --print --no-autostart");
    let stdout = text(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line == "Nothing removed: --print changes nothing."),
        "{stdout}"
    );

    assert_eq!(user.manager.calls(), Vec::<Vec<String>>::new());
    assert!(!platform::registered(&user.manager, &user.home()));
    assert!(!definition.exists(), "{}", definition.display());
}

/// The registration starts the daemon heli finds by default, so while a
/// variable that moves heli's locations is set, setup refuses to register
/// or remove it, with one line saying why, before it does anything else.
#[test]
fn setup_refuses_autostart_for_other_locations() {
    let user = User::new();
    let elsewhere = user.fixture.path("elsewhere");
    for (var, flag) in [
        ("HELI_STATE_DIR", "--autostart"),
        ("HELI_ENDPOINT", "--autostart"),
        ("HELI_POOL", "--autostart"),
        ("HELI_CONFIG", "--autostart"),
        ("HELI_STATE_DIR", "--no-autostart"),
    ] {
        let output = user
            .setup_command(&[flag])
            .env(var, &elsewhere)
            .output()
            .expect("run heli setup");
        assert_eq!(output.status.code(), Some(1), "{var} {flag}: {output:?}");
        assert_eq!(text(&output.stdout), "", "{var} {flag}: nothing else done");
        let stderr = text(&output.stderr);
        let verb = if flag == "--autostart" {
            "register"
        } else {
            "remove"
        };
        assert_eq!(
            stderr,
            format!(
                "cargo-heli: cannot {verb} autostart: {var} is set, and the registration \
                 starts the daemon heli finds by default; unset it first\n"
            )
        );
    }
    assert_eq!(user.manager.calls(), Vec::<Vec<String>>::new());
}

/// When the service manager refuses to replace a registration, clients no
/// longer start the daemon through it: setup dropped its record before it
/// began.
#[test]
fn a_failed_registration_is_not_used() {
    let (user, _names) = User::registered();
    let elsewhere = heli_copy(&user.fixture, "elsewhere");
    let mut setup = with_program(&user.setup_command(&["--autostart"]), &elsewhere);
    let output = setup
        .env("FAKE_MANAGER_FAIL", platform::REGISTER_VERB)
        .output()
        .expect("run the other heli's setup");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    user.manager.clear_calls();

    let output = user.heli(&["ensure"]).output().expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    assert_eq!(user.manager.calls(), Vec::<Vec<String>>::new());
}

/// When the service manager refuses the registration, setup fails with one
/// line naming the command and what it said, and clients do not start the
/// daemon through the registration.
#[test]
fn setup_fails_when_the_service_manager_does() {
    let user = User::new();
    let output = user
        .setup_command(&["--autostart"])
        .env("FAKE_MANAGER_FAIL", platform::REGISTER_VERB)
        .output()
        .expect("run heli setup");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = text(&output.stdout);
    assert!(!stdout.contains(REGISTERED), "{stdout}");
    let stderr = text(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("cargo-heli: cannot register autostart: `")
            && stderr.contains(platform::REGISTER_VERB)
            && stderr
                .trim_end()
                .ends_with(&format!("fake-manager: {} failed", platform::REGISTER_VERB)),
        "{stderr}"
    );
    user.manager.clear_calls();

    let output = user.heli(&["ensure"]).output().expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    assert_eq!(
        user.manager.calls(),
        Vec::<Vec<String>>::new(),
        "the client started the daemon itself"
    );
}

/// Once the daemon is registered, a client that finds no daemon starts it
/// through the registration (on Windows, outside the job of the agent that
/// ran the client) instead of starting it itself, and uses it. What the
/// service manager prints stays out of the client's output.
#[test]
fn a_client_starts_the_daemon_through_the_registration() {
    let (user, names) = User::registered();
    let mut ensure = user.heli(&["ensure"]);
    user.manager
        .apply_starting(&mut ensure, heli_binary(), platform::START_ARGS);
    let output = ensure.output().expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    assert_eq!(text(&output.stdout), "");
    assert_eq!(text(&output.stderr), "");
    assert_eq!(user.manager.calls(), vec![names.start()]);
    assert!(user.fixture.daemon_pid().is_some(), "a daemon runs");

    let echo = user.builder.echo_args();
    let output = user
        .heli(&["exec", "--"])
        .arg(&echo)
        .arg("through")
        .output()
        .expect("run heli exec");
    assert_success(&output, "cargo heli exec");
    assert_eq!(text(&output.stdout), "arg: \"through\"\n");
    assert_eq!(
        support::without_footer(&output.stderr),
        "echo-args: stderr\n"
    );
    assert_eq!(user.manager.calls(), vec![names.start()], "started once");
}

/// A client that asked the registration to start the daemon never starts
/// one itself: when none comes up (here the service manager starts
/// nothing), it runs the command directly after 5 s, with one warning.
#[test]
fn a_client_waits_for_the_daemon_the_registration_starts() {
    let (user, names) = User::registered();
    let echo = user.builder.echo_args();
    let output = user
        .heli(&["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli exec");
    assert_success(&output, "cargo heli exec");
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        "cargo-heli: warning: the heli daemon did not listen within 5 s of its start; \
         running the command directly\necho-args: stderr\n"
    );
    assert_eq!(user.manager.calls(), vec![names.start()]);
    assert_eq!(user.fixture.daemon_pid(), None, "no daemon was started");
}

/// When the service manager refuses to start the registration (it was
/// removed behind heli's back), the client starts the daemon itself, as
/// without one.
#[test]
fn a_refused_start_means_the_client_starts_the_daemon_itself() {
    let (user, names) = User::registered();
    let echo = user.builder.echo_args();
    let output = user
        .heli(&["exec", "--"])
        .arg(&echo)
        .env("FAKE_MANAGER_FAIL", platform::START_VERB)
        .output()
        .expect("run heli exec");
    assert_success(&output, "cargo heli exec");
    assert_eq!(
        support::without_footer(&output.stderr),
        "echo-args: stderr\n"
    );
    assert_eq!(user.manager.calls(), vec![names.start()]);
    assert!(user.fixture.daemon_pid().is_some(), "a daemon runs");
}

/// A service manager that does not answer within 5 s counts as a refusal:
/// the client gives up on it and starts the daemon itself.
#[test]
fn a_service_manager_that_does_not_answer_counts_as_a_refusal() {
    let (user, names) = User::registered();
    let echo = user.builder.echo_args();
    let output = user
        .heli(&["exec", "--"])
        .arg(&echo)
        .env("FAKE_MANAGER_SLEEP_MS", "60000")
        .output()
        .expect("run heli exec");
    assert_success(&output, "cargo heli exec");
    assert_eq!(
        support::without_footer(&output.stderr),
        "echo-args: stderr\n"
    );
    assert_eq!(user.manager.calls(), vec![names.start()]);
}

/// A start through the registration counts toward the nesting limit as a
/// start by the client does: the service manager is one level deeper than
/// the client, so from a client deep enough the daemon would be nested too
/// deep, and none comes up.
#[test]
fn the_nesting_limit_bounds_a_start_through_the_registration() {
    let (user, names) = User::registered();
    let mut ensure = user.heli(&["ensure"]);
    user.manager
        .apply_starting(&mut ensure, heli_binary(), platform::START_ARGS);
    let output = ensure
        .env("HELI_DEPTH", platform::TOO_DEEP)
        .output()
        .expect("run heli ensure");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(user.manager.calls(), vec![names.start()]);
    assert_eq!(user.fixture.daemon_pid(), None);
}

/// The registration starts the program setup registered, at the default
/// locations. Another program (heli installed elsewhere), or a client whose
/// locations differ (here its configuration), starts its own daemon.
#[test]
fn a_client_of_another_program_or_locations_starts_its_own_daemon() {
    let (user, _names) = User::registered();
    let elsewhere = heli_copy(&user.fixture, "elsewhere");
    let output = with_program(&user.heli(&["ensure"]), &elsewhere)
        .output()
        .expect("run the other heli's ensure");
    assert_success(&output, "the other heli's ensure");
    assert_eq!(user.manager.calls(), Vec::<Vec<String>>::new());
    assert!(user.fixture.daemon_pid().is_some(), "a daemon runs");
    user.fixture.kill_daemon();

    // In the daemon log's clock: seconds since the epoch.
    let asked = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    let output = user
        .heli(&["ensure"])
        .env("HELI_CONFIG", user.fixture.path("other.toml"))
        .output()
        .expect("run heli ensure");
    assert!(
        output.status.success(),
        "cargo heli ensure with another configuration, at {asked} s: {output:?}\n\
         the daemon's log:\n{}",
        user.fixture.daemon_log()
    );
    assert_eq!(user.manager.calls(), Vec::<Vec<String>>::new());
    assert!(user.fixture.daemon_pid().is_some(), "a daemon runs");
}

/// The Windows logon task runs `cargo heli daemon --detach`, which starts
/// the daemon even when its state directory does not exist yet.
#[test]
fn daemon_detach_creates_the_state_directory() {
    let fixture = Fixture::new("basic");
    assert!(!fixture.state_dir().exists());
    let output = fixture
        .heli(["daemon", "--detach"])
        .stdin(Stdio::null())
        .output()
        .expect("run heli daemon --detach");
    assert_success(&output, "cargo heli daemon --detach");
    assert!(fixture.daemon_pid().is_some(), "a daemon runs");
    assert!(fixture.state_dir().is_dir());
}

/// The definition is one the service manager itself accepts, as its own
/// tool checks it, without registering anything: Task Scheduler's parser,
/// `systemd-analyze --user verify`, `plutil -lint`.
#[test]
fn the_definition_passes_the_service_managers_own_check() {
    let user = User::new();
    let output = user.setup(&["--autostart"]);
    assert_success(&output, "cargo heli setup --autostart");
    platform::check(&platform::definition(&user.home()), &user.home());
}
