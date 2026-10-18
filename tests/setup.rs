//! `cargo heli ensure` and `cargo heli setup` (DESIGN.md §4, "Reach"): the
//! daemon on demand, and the environment that lets a bare cargo draw from
//! its pool. `tests/bare_cargo.rs` covers what a bare cargo then does.
//!
//! Every `setup` here runs in the fixture's temporary home
//! ([`Fixture::setup`]), so no test edits the user's real settings or
//! shell profile.

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::{Fixture, assert_success};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// How many daemons the fixture's daemon log says have started.
fn daemon_starts(fixture: &Fixture) -> usize {
    std::fs::read_dir(fixture.state_dir())
        .expect("the state directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("daemon") && name.ends_with(".log")
        })
        .map(|path| std::fs::read_to_string(path).expect("read the daemon's log"))
        .flat_map(|log| {
            log.lines()
                .filter(|line| line.contains("] started, pid "))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .count()
}

/// `ensure` starts the daemon when none runs, and prints nothing (a
/// session-start hook's output would land in the agent's context). Run
/// again, it finds that daemon and returns at once, starting no other.
#[test]
fn ensure_starts_the_daemon_once() {
    let fixture = Fixture::new("basic");
    assert!(fixture.daemon_pid().is_none(), "no daemon yet");

    let output = fixture.heli(["ensure"]).output().expect("run heli ensure");
    assert_success(&output, "cargo heli ensure");
    assert_eq!(text(&output.stdout), "", "nothing on stdout");
    assert_eq!(text(&output.stderr), "", "nothing on stderr");
    let daemon = fixture.daemon_pid().expect("ensure started a daemon");

    for _ in 0..2 {
        let started = Instant::now();
        let output = fixture.heli(["ensure"]).output().expect("run heli ensure");
        let took = started.elapsed();
        assert_success(&output, "cargo heli ensure, again");
        assert_eq!(text(&output.stdout), "");
        assert_eq!(text(&output.stderr), "");
        assert!(
            took < Duration::from_secs(2),
            "ensure took {took:?} with a daemon running"
        );
    }
    assert_eq!(fixture.daemon_pid(), Some(daemon), "the same daemon");
    assert_eq!(daemon_starts(&fixture), 1, "one daemon ever started");
}

/// When no daemon can start (here: its state directory cannot be created),
/// `ensure` fails with one line saying why.
#[test]
fn ensure_says_why_no_daemon_runs() {
    let fixture = Fixture::new("basic");
    let not_a_dir = fixture.path("not-a-directory");
    std::fs::write(&not_a_dir, "").expect("create a file");
    let output = fixture
        .heli(["ensure"])
        .env("HELI_STATE_DIR", &not_a_dir)
        .output()
        .expect("run heli ensure");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    let stderr = text(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("cargo-heli: cannot create the heli state directory"),
        "{stderr}"
    );
}

/// The value `setup` offers: the fixture's pool as `CARGO_MAKEFLAGS` names
/// it (on Windows the semaphore, elsewhere `fifo:<path>`), as every heli run
/// gets it.
fn pool_makeflags(fixture: &Fixture) -> String {
    let pool = fixture.pool().into_string().expect("a Unicode pool name");
    if cfg!(windows) {
        format!("-j --jobserver-auth={pool}")
    } else {
        format!("-j --jobserver-auth=fifo:{pool}")
    }
}

/// The lines a profile ends with once `setup` has written `makeflags` to it,
/// for a shell of the sh family.
fn sh_lines(makeflags: &str) -> String {
    format!(
        "# Added by `cargo heli setup`: a bare cargo draws from heli's host pool.\n\
         export CARGO_MAKEFLAGS='{makeflags}'\n"
    )
}

/// What `setup`'s output shows under the line `header`: the lines up to
/// the next empty one.
fn shown(stdout: &str, header: &str) -> String {
    let mut lines = stdout.lines();
    assert!(
        lines.any(|line| line == header),
        "no line {header:?} in:\n{stdout}"
    );
    lines
        .take_while(|line| !line.is_empty())
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Runs `command` with `answers` on its stdin, then end of input.
fn answered(mut command: Command, answers: &str) -> Output {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("run heli setup");
    child
        .stdin
        .take()
        .expect("a stdin pipe")
        .write_all(answers.as_bytes())
        .expect("answer");
    child.wait_with_output().expect("wait for heli setup")
}

/// A home with Claude Code's settings (a model and a variable of the
/// user's) and a bash profile.
struct Home {
    settings: PathBuf,
    profile: PathBuf,
}

impl Home {
    fn new(fixture: &Fixture) -> Self {
        let home = fixture.home();
        let claude = home.join(".claude");
        std::fs::create_dir_all(&claude).expect("create .claude");
        let settings = claude.join("settings.json");
        std::fs::write(&settings, r#"{"model":"opus","env":{"FOO":"bar"}}"#)
            .expect("write the settings");
        let profile = home.join(".bashrc");
        std::fs::write(&profile, "alias ll='ls -l'\n").expect("write the profile");
        Self { settings, profile }
    }

    /// The files' contents.
    fn contents(&self) -> (String, String) {
        (read(&self.settings), read(&self.profile))
    }

    fn settings_header(&self) -> String {
        format!(
            "Claude Code settings {} would become:",
            self.settings.display()
        )
    }

    fn profile_header(&self) -> String {
        format!("Shell profile {} would end with:", self.profile.display())
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// `setup --print` shows the pool's variable, and for each file it would
/// edit, exactly what it would write: the whole new settings file, keeping
/// the user's own settings in their order, and the lines it would add at the
/// end of the shell profile. It asks nothing and writes nothing.
#[test]
fn setup_print_shows_what_it_would_write() {
    let fixture = Fixture::new("basic");
    let home = Home::new(&fixture);
    let before = home.contents();
    let makeflags = pool_makeflags(&fixture);

    let output = fixture
        .setup(Some("/bin/bash"), ["--print"])
        .output()
        .expect("run heli setup --print");
    assert_success(&output, "cargo heli setup --print");
    let stdout = text(&output.stdout);
    assert_eq!(
        stdout.lines().next(),
        Some(format!("CARGO_MAKEFLAGS={makeflags}").as_str()),
        "{stdout}"
    );
    assert_eq!(
        shown(&stdout, &home.settings_header()),
        format!(
            "{{\n  \"model\": \"opus\",\n  \"env\": {{\n    \"FOO\": \"bar\",\n    \
             \"CARGO_MAKEFLAGS\": \"{makeflags}\"\n  }}\n}}\n"
        )
    );
    assert_eq!(shown(&stdout, &home.profile_header()), sh_lines(&makeflags));
    assert!(!stdout.contains("[y/N]"), "it asks nothing: {stdout}");
    assert_eq!(home.contents(), before, "it writes nothing");
}

/// Answered yes, `setup` writes exactly what `--print` showed. It shows the
/// answers it read, which no terminal echoed, and ignores the byte order
/// mark PowerShell can put before them. Run again, it finds both files set
/// (and the Cargo configuration, also answered yes), and asks nothing.
#[test]
fn setup_writes_what_it_showed_once_the_user_agrees() {
    let fixture = Fixture::new("basic");
    let home = Home::new(&fixture);
    let (_, profile_before) = home.contents();
    let output = fixture
        .setup(Some("/bin/bash"), ["--print"])
        .output()
        .expect("run heli setup --print");
    assert_success(&output, "cargo heli setup --print");
    let printed = text(&output.stdout);
    let settings = shown(&printed, &home.settings_header());
    let profile_end = shown(&printed, &home.profile_header());

    // The third answer is for the Cargo configuration (`tests/store.rs`), the
    // fourth, on Windows, for the user environment (the fixture's stand-in).
    let output = answered(
        fixture.setup(Some("/bin/bash"), Vec::<String>::new()),
        if cfg!(windows) {
            "\u{feff}y\nyes\ny\ny\n"
        } else {
            "\u{feff}y\nyes\ny\n"
        },
    );
    assert_success(&output, "cargo heli setup");
    let stdout = text(&output.stdout);
    for (path, answer) in [(&home.settings, "y"), (&home.profile, "yes")] {
        let asked = format!("Write {}? [y/N] {answer}\nWritten.\n", path.display());
        assert!(
            stdout.contains(&asked),
            "it asks before writing {}: {stdout}",
            path.display()
        );
    }
    assert_eq!(read(&home.settings), settings);
    assert_eq!(
        read(&home.profile),
        format!("{profile_before}{profile_end}")
    );

    let written = home.contents();
    let output = answered(
        fixture.setup(Some("/bin/bash"), Vec::<String>::new()),
        "y\ny\n",
    );
    assert_success(&output, "cargo heli setup, again");
    let stdout = text(&output.stdout);
    assert!(!stdout.contains("[y/N]"), "nothing left to ask: {stdout}");
    assert_eq!(
        stdout.matches("already sets CARGO_MAKEFLAGS").count(),
        if cfg!(windows) { 3 } else { 2 },
        "{stdout}"
    );
    assert_eq!(home.contents(), written, "nothing written twice");
}

/// Without a yes, `setup` writes nothing: not on "n", not at the end of its
/// input, and not when it has no input at all. It does not create a settings
/// file either.
#[test]
fn setup_writes_nothing_without_consent() {
    let fixture = Fixture::new("basic");
    let home = Home::new(&fixture);
    std::fs::remove_file(&home.settings).expect("remove the settings");
    let profile = read(&home.profile);

    let output = answered(
        fixture.setup(Some("/bin/bash"), Vec::<String>::new()),
        "n\n",
    );
    assert_success(&output, "cargo heli setup, answered no");
    // The settings, the profile and the Cargo configuration, and on Windows
    // the user environment.
    assert_eq!(
        text(&output.stdout).matches("[y/N]").count(),
        if cfg!(windows) { 4 } else { 3 }
    );
    let output = fixture
        .setup(Some("/bin/bash"), Vec::<String>::new())
        .output()
        .expect("run heli setup without input");
    assert_success(&output, "cargo heli setup without input");

    assert!(!home.settings.exists(), "no settings file was created");
    assert_eq!(read(&home.profile), profile);
    assert!(
        !fixture.home().join(".cargo").exists(),
        "no Cargo configuration was created"
    );
}

/// The profile is the one of the shell `SHELL` names, in its syntax:
/// `.zshrc`, fish's `config.fish`, `.profile` for any other shell (on
/// Windows also Git Bash's `bash.exe`). Without `SHELL`, there is none, and
/// setup says how to set the variable by hand.
#[test]
fn setup_finds_the_profile_of_each_shell() {
    let fixture = Fixture::new("basic");
    let makeflags = pool_makeflags(&fixture);
    let fish = format!(
        "# Added by `cargo heli setup`: a bare cargo draws from heli's host pool.\n\
         set -gx CARGO_MAKEFLAGS '{makeflags}'\n"
    );
    let mut shells = vec![
        ("/usr/bin/zsh", vec![".zshrc"], sh_lines(&makeflags)),
        (
            "/usr/local/bin/fish",
            vec![".config", "fish", "config.fish"],
            fish.clone(),
        ),
        ("/bin/sh", vec![".profile"], sh_lines(&makeflags)),
    ];
    if cfg!(windows) {
        shells.push((
            r"C:\Program Files\Git\bin\bash.exe",
            vec![".bashrc"],
            sh_lines(&makeflags),
        ));
    }
    for (shell, profile, lines) in shells {
        let output = fixture
            .setup(Some(shell), ["--print"])
            .output()
            .expect("run heli setup --print");
        assert_success(&output, shell);
        let path = profile
            .iter()
            .fold(fixture.home(), |path, part| path.join(part));
        let header = format!("Shell profile {} would end with:", path.display());
        assert_eq!(shown(&text(&output.stdout), &header), lines, "{shell}");
    }

    let output = fixture
        .setup(None, ["--print"])
        .output()
        .expect("run heli setup --print");
    assert_success(&output, "setup without SHELL");
    let stdout = text(&output.stdout);
    assert!(
        !stdout.contains("Shell profile ") && stdout.contains("Shell profile: none"),
        "{stdout}"
    );
    let by_hand = if cfg!(windows) {
        format!("[Environment]::SetEnvironmentVariable('CARGO_MAKEFLAGS', '{makeflags}', 'User')")
    } else {
        format!("export CARGO_MAKEFLAGS='{makeflags}'")
    };
    assert!(
        stdout.lines().any(|line| line == by_hand),
        "how to set it by hand: {stdout}"
    );
    let no_claude = format!(
        "Claude Code settings: none here ({} does not exist).",
        fixture.home().join(".claude").display()
    );
    assert!(
        stdout.lines().any(|line| line == no_claude),
        "without Claude Code, no settings to offer: {stdout}"
    );

    // A profile whose directory does not exist yet is written all the same.
    let output = answered(
        fixture.setup(Some("/usr/local/bin/fish"), Vec::<String>::new()),
        "y\n",
    );
    assert_success(&output, "cargo heli setup for fish");
    let config = fixture
        .home()
        .join(".config")
        .join("fish")
        .join("config.fish");
    assert_eq!(read(&config), fish);
}

/// Claude Code's configuration directory is `CLAUDE_CONFIG_DIR` when set.
#[test]
fn setup_follows_claude_config_dir() {
    let fixture = Fixture::new("basic");
    let config = fixture.home().join("elsewhere");
    std::fs::create_dir_all(&config).expect("create the configuration directory");
    let output = fixture
        .setup(None, ["--print"])
        .env("CLAUDE_CONFIG_DIR", &config)
        .output()
        .expect("run heli setup --print");
    assert_success(&output, "cargo heli setup --print");
    let header = format!(
        "Claude Code settings {} would become:",
        config.join("settings.json").display()
    );
    let makeflags = pool_makeflags(&fixture);
    assert_eq!(
        shown(&text(&output.stdout), &header),
        format!("{{\n  \"env\": {{\n    \"CARGO_MAKEFLAGS\": \"{makeflags}\"\n  }}\n}}\n")
    );
}

/// A file setup was told to write but cannot (here: read-only) fails the
/// command, with one line saying which; the other file is still written.
#[test]
fn setup_fails_when_it_cannot_write() {
    let fixture = Fixture::new("basic");
    let home = Home::new(&fixture);
    let mut permissions = std::fs::metadata(&home.settings)
        .expect("the settings")
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&home.settings, permissions).expect("make the settings read-only");
    let settings = read(&home.settings);

    let output = answered(
        fixture.setup(Some("/bin/bash"), Vec::<String>::new()),
        "y\ny\n",
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = text(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with(&format!(
            "cargo-heli: cannot write {}: ",
            home.settings.display()
        )),
        "{stderr}"
    );
    assert_eq!(read(&home.settings), settings);
    assert!(
        read(&home.profile).ends_with(&sh_lines(&pool_makeflags(&fixture))),
        "the profile is written"
    );
}

/// Lines an earlier setup wrote with another value are replaced: they go,
/// and the new ones end the file. A profile without a final line break gets
/// one before them.
#[test]
fn setup_replaces_what_an_earlier_setup_wrote() {
    let fixture = Fixture::new("basic");
    let home = Home::new(&fixture);
    std::fs::write(
        &home.profile,
        "first\n\
         # Added by `cargo heli setup`: a bare cargo draws from heli's host pool.\n\
         export CARGO_MAKEFLAGS='-j --jobserver-auth=an-old-pool'\n\
         last",
    )
    .expect("write the profile");
    let output = answered(
        fixture.setup(Some("/bin/bash"), Vec::<String>::new()),
        "n\ny\n",
    );
    assert_success(&output, "cargo heli setup");
    let lines = sh_lines(&pool_makeflags(&fixture));
    assert_eq!(shown(&text(&output.stdout), &home.profile_header()), lines);
    assert_eq!(read(&home.profile), format!("first\nlast\n{lines}"));
}

/// Settings that are not a JSON object, or whose `env` is not one, are left
/// as they are, with the reason; setup still offers the profile.
#[test]
fn setup_leaves_settings_it_cannot_edit_alone() {
    for (settings, reason) in [
        ("[1, 2]", "it is not a JSON object"),
        (r#"{"env": "none"}"#, "its `env` is not a JSON object"),
        ("{ // a comment\n}", "it is not JSON"),
    ] {
        let fixture = Fixture::new("basic");
        let home = Home::new(&fixture);
        std::fs::write(&home.settings, settings).expect("write the settings");
        let output = answered(
            fixture.setup(Some("/bin/bash"), Vec::<String>::new()),
            "y\ny\n",
        );
        assert_success(&output, "cargo heli setup");
        let stdout = text(&output.stdout);
        let left = format!(
            "Claude Code settings {} is left as it is: {reason}",
            home.settings.display()
        );
        assert!(stdout.contains(&left), "{settings}: {stdout}");
        // The profile and the Cargo configuration, and on Windows the user
        // environment.
        assert_eq!(
            stdout.matches("[y/N]").count(),
            if cfg!(windows) { 3 } else { 2 },
            "{stdout}"
        );
        assert_eq!(read(&home.settings), settings);
    }
}
