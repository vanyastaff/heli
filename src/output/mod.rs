//! What the client prints of a run: the tool's output in the chosen mode,
//! then heli's closing line (`DESIGN.md` §5, "Output").
//!
//! **Modes** ([`Mode`]), chosen by [`choose`]: `--output` or `-q` before the
//! subcommand, else `HELI_OUTPUT` ([`VAR`]), else `[output] mode` in the
//! configuration (`crate::config`), else full when the client's stdout is a
//! terminal and compact when it is not.
//!
//! **A plan per run** ([`plan`]): from the command line and the mode, the
//! client decides before the run starts which view shows its output, and
//! what that view adds to the run's arguments and environment. The daemon
//! runs and records what the plan says, so the run log holds what the tool
//! printed for that view (`crate::runlog`). Views:
//!
//! - **Pass-through**: the output as the tool writes it, byte for byte.
//!   Every run in full mode; in compact and quiet mode, every run no other
//!   view takes (cargo subcommands heli knows nothing of, `run`, `fmt`,
//!   mutate runs), and every cargo run whose arguments already choose
//!   cargo's output (`--message-format`, `-q`, `-v`: [`cargo::sets_output`]).
//! - **Cargo's diagnostics** ([`cargo`]): compile and verify runs
//!   (`crate::class`) of build, check, clippy and doc, in compact and quiet
//!   mode. heli adds `--quiet --message-format json` and renders the
//!   diagnostics from cargo's JSON.
//! - **Tests**, test runs in compact and quiet mode, both their phases
//!   (`crate::daemon::test_run`): [`nextest`] for `nextest run`, which adds
//!   nextest's own controls the user did not set; [`libtest`] for `test`,
//!   which adds cargo's flags as above and keeps libtest's `failures:`
//!   sections. Both render cargo's JSON as for a compile run.
//! - **Head and tail** ([`exec`]): `cargo heli exec` in compact and quiet
//!   mode. heli adds nothing, and shows the first and the last lines.
//!
//! **Quiet mode** holds a rendered view's output back until the run ends,
//! and heli's own lines about the run with it ([`Printer::note`]): on
//! success the client prints one line instead ([`Printer::exited`]); on
//! failure, the held output and the footer, as compact mode would have.
//! Pass-through output is never held.
//!
//! **Adding a view**: a [`View`] variant; its case in
//! [`view_of`], which decides from the run's class when it applies; what it
//! adds to the command in [`plan`]; and a [`Render`] that turns the run's
//! output into what the client prints and returns the facts for quiet
//! mode's line (`812 passed, 3 skipped`). Quiet mode and the closing line
//! then work for it unchanged.
//!
//! **The closing line**, on stderr, starting a line of its own:
//! - the footer, after every run: `cargo-heli: r17 exit 0 in 41.2 s
//!   (output: cargo heli log r17)` (on Unix `signal <n>` for a run a
//!   signal killed);
//! - instead, after a quiet run that succeeded: `ok  build -p foo:
//!   3 warnings, 41.2 s  [r17]`, the command without what heli added.
//!
//! **Colours.** Runs write to pipes, so cargo would print no colours even
//! when the client is on a terminal. When a run's output passes through to
//! a client whose stdout and stderr are both terminals, the run gets
//! `CARGO_TERM_COLOR=always` (cargo, nextest and every cargo the run starts
//! read it), unless the client's environment sets `CARGO_TERM_COLOR` or
//! `NO_COLOR` already.

pub mod cargo;
pub mod exec;
pub mod libtest;
pub mod nextest;

use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;

use crate::class::Class;
use crate::cli::Invocation;
use crate::protocol::Channel;

/// Selects the output mode, below `--output` and `-q`.
pub const VAR: &str = "HELI_OUTPUT";

/// Cargo's colour setting, which heli sets for a terminal (see the module
/// docs).
const COLOR_VAR: &str = "CARGO_TERM_COLOR";

/// How much of a run's output the client prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The tool's output as it streams.
    Full,
    /// What an agent needs to act: the verdict, the failures, the findings.
    Compact,
    /// One line on success; compact output on failure.
    Quiet,
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "full" => Ok(Self::Full),
            "compact" => Ok(Self::Compact),
            "quiet" => Ok(Self::Quiet),
            _ => Err(format!(
                "`{text}` is not an output mode: full, compact or quiet"
            )),
        }
    }
}

/// The mode of a run: `flag`, else [`VAR`], else `configured()`, else full
/// on a terminal and compact without. The error says why [`VAR`] is not a
/// mode.
pub fn choose(
    flag: Option<Mode>,
    configured: impl FnOnce() -> Option<Mode>,
) -> Result<Mode, String> {
    if let Some(mode) = flag {
        return Ok(mode);
    }
    if let Some(value) = std::env::var_os(VAR).filter(|value| !value.is_empty()) {
        return value
            .to_str()
            .unwrap_or_default()
            .parse()
            .map_err(|error| format!("{VAR}={}: {error}", value.to_string_lossy()));
    }
    if let Some(mode) = configured() {
        return Ok(mode);
    }
    Ok(if io::stdout().is_terminal() {
        Mode::Full
    } else {
        Mode::Compact
    })
}

/// How the output of one run is shown, decided before it starts.
pub struct Plan {
    /// The run's arguments after the program: the user's, and what the view
    /// adds.
    pub args: Vec<OsString>,
    /// What the view sets in the run's environment.
    pub env: Vec<(&'static str, OsString)>,
    view: View,
    mode: Mode,
    /// The command as the user gave it, for quiet mode's line.
    command: String,
}

/// The views a run's output can be shown in (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    PassThrough,
    Cargo(cargo::Warnings),
    Tests(Runner),
    Exec,
}

/// The tool that runs a test run's tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runner {
    /// `cargo test`.
    Libtest,
    /// `cargo nextest run`.
    Nextest,
}

/// The plan for running `invocation` in `mode`.
pub fn plan(invocation: &Invocation, mode: Mode) -> Plan {
    let view = match mode {
        Mode::Full => View::PassThrough,
        Mode::Compact | Mode::Quiet => view_of(invocation),
    };
    let mut env = Vec::new();
    let args = match view {
        View::Cargo(_) | View::Tests(Runner::Libtest) => {
            cargo::with_json_messages(&invocation.args)
        }
        View::Tests(Runner::Nextest) => nextest::with_controls(&invocation.args, |name| {
            std::env::var_os(name).is_some_and(|value| !value.is_empty())
        }),
        View::Exec => invocation.args.clone(),
        View::PassThrough => {
            if colours_wanted() {
                env.push((COLOR_VAR, OsString::from("always")));
            }
            invocation.args.clone()
        }
    };
    // A cargo subcommand is named as in `cargo heli`, without `cargo`; any
    // other command by its program's name.
    let program = invocation.exec.then(|| {
        Path::new(&invocation.program)
            .file_stem()
            .unwrap_or(&invocation.program)
            .to_os_string()
    });
    let command = program
        .iter()
        .chain(&invocation.args)
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    Plan {
        args,
        env,
        view,
        mode,
        command,
    }
}

/// The view that shows `invocation`'s output in compact and quiet mode,
/// from the run's class. Compile and verify runs show cargo's diagnostics,
/// their warnings counted or listed by lint, when cargo can report them as
/// JSON for the subcommand ([`cargo::reports_json`]) and the user's
/// arguments leave cargo's output to heli ([`cargo::sets_output`]); test
/// runs show their tests ([`tests_view`]); `cargo heli exec` shows its head
/// and tail. Every other run passes through.
fn view_of(invocation: &Invocation) -> View {
    let warnings = match invocation.class {
        Class::Compile => cargo::Warnings::Counts,
        Class::Verify => cargo::Warnings::ByLint,
        Class::Test => return tests_view(&invocation.args),
        Class::Exec if invocation.exec => return View::Exec,
        Class::Mutate | Class::Exec => return View::PassThrough,
    };
    if cargo::reports_json(&invocation.args) && !cargo::sets_output(&invocation.args) {
        View::Cargo(warnings)
    } else {
        View::PassThrough
    }
}

/// The view of a test run, `cargo <args>`: nextest's always (heli adds each
/// of its controls the user did not set), libtest's unless the user's
/// arguments choose cargo's output.
fn tests_view(args: &[OsString]) -> View {
    let Some((subcommand, rest)) = cargo::subcommand(args) else {
        return View::PassThrough;
    };
    match subcommand.to_str().unwrap_or_default() {
        "nextest" if rest.first().is_some_and(|run| run == "run" || run == "r") => {
            View::Tests(Runner::Nextest)
        }
        "test" | "t" if !cargo::sets_output(args) => View::Tests(Runner::Libtest),
        _ => View::PassThrough,
    }
}

/// Whether a run whose output passes through should print colours.
fn colours_wanted() -> bool {
    let unset = |name: &str| std::env::var_os(name).is_none_or(|value| value.is_empty());
    io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && unset(COLOR_VAR)
        && unset("NO_COLOR")
}

impl Plan {
    /// The printer for the run of this plan.
    pub fn printer(&self) -> Printer {
        let render: Box<dyn Render> = match self.view {
            View::PassThrough => Box::new(PassThrough),
            View::Cargo(warnings) => Box::new(cargo::Diagnostics::new(warnings)),
            View::Tests(Runner::Nextest) => Box::new(nextest::Nextest::new()),
            View::Tests(Runner::Libtest) => Box::new(libtest::Libtest::new()),
            View::Exec => Box::new(exec::HeadTail::new()),
        };
        let quiet = self.mode == Mode::Quiet && self.view != View::PassThrough;
        Printer {
            render,
            sink: Sink::new(quiet),
            quiet,
            command: self.command.clone(),
        }
    }
}

/// Turns a run's output into what the client prints. A renderer sees all of
/// one run's output, in order.
pub trait Render {
    /// Takes what the run wrote to `channel`, and writes what is ready to
    /// `out`.
    fn output(&mut self, channel: Channel, bytes: &[u8], out: &mut Sink);

    /// The run is over: writes what was held back to `out`, and returns the
    /// facts quiet mode's line shows, for example `3 warnings`. `run` is
    /// the run's id, for pointers to `cargo heli log`.
    fn finish(&mut self, run: &str, out: &mut Sink) -> Vec<String>;
}

/// The output as the tool wrote it.
struct PassThrough;

impl Render for PassThrough {
    fn output(&mut self, channel: Channel, bytes: &[u8], out: &mut Sink) {
        out.write(channel, bytes);
    }

    fn finish(&mut self, _run: &str, _out: &mut Sink) -> Vec<String> {
        Vec::new()
    }
}

/// One stream's output, cut into lines: each with its line end, the start
/// of a line waiting for the rest.
#[derive(Default)]
struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    /// The lines `bytes` completes, in order.
    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut from = self.pending.len();
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending[from..].iter().position(|&byte| byte == b'\n') {
            lines.push(self.pending.drain(..=from + end).collect());
            from = 0;
        }
        lines
    }

    /// The stream's last bytes, when they do not end a line.
    fn rest(&mut self) -> Option<Vec<u8>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

/// Where a renderer writes: the client's stdout and stderr, or in quiet
/// mode a buffer that is printed only if the run fails.
pub struct Sink {
    held: Option<Vec<(Channel, Vec<u8>)>>,
    /// Whether what was written to stderr so far ends a line.
    line_ended: bool,
}

impl Sink {
    fn new(hold: bool) -> Self {
        Self {
            held: hold.then(Vec::new),
            line_ended: true,
        }
    }

    /// Writes `bytes` to `channel`.
    pub fn write(&mut self, channel: Channel, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if channel == Channel::Stderr {
            self.line_ended = bytes.ends_with(b"\n");
        }
        match &mut self.held {
            Some(held) => match held.last_mut() {
                Some((last, held)) if *last == channel => held.extend_from_slice(bytes),
                _ => held.push((channel, bytes.to_vec())),
            },
            None => emit(channel, bytes),
        }
    }

    /// Writes `text` as a line of its own on stderr.
    pub fn line(&mut self, text: &str) {
        let start = if self.line_ended { "" } else { "\n" };
        self.write(Channel::Stderr, format!("{start}{text}\n").as_bytes());
    }

    /// Prints what was held back, and holds nothing from now on.
    fn release(&mut self) {
        for (channel, bytes) in self.held.take().unwrap_or_default() {
            emit(channel, &bytes);
        }
    }

    /// Drops what was held back, and holds nothing from now on.
    fn discard(&mut self) {
        self.held = None;
        self.line_ended = true;
    }
}

/// Prints `bytes` on the client's stream for `channel`, at once.
fn emit(channel: Channel, bytes: &[u8]) {
    match channel {
        Channel::Stdout => print(&mut io::stdout().lock(), bytes),
        Channel::Stderr => print(&mut io::stderr().lock(), bytes),
    }
}

/// Prints one run's output as the plan says, then the closing line.
pub struct Printer {
    render: Box<dyn Render>,
    sink: Sink,
    quiet: bool,
    command: String,
}

impl Printer {
    /// Takes what the run wrote to `channel`.
    pub fn output(&mut self, channel: Channel, bytes: &[u8]) {
        self.render.output(channel, bytes, &mut self.sink);
    }

    /// Prints a line of heli's own about the run (`waiting for …`) on
    /// stderr; quiet mode holds it back with the run's output.
    pub fn note(&mut self, text: &str) {
        self.sink.line(text);
    }

    /// Run `run` exited with `code` (or on Unix of `signal`) after
    /// `elapsed`: prints the rest of its output and the closing line.
    pub fn exited(mut self, run: &str, code: i32, signal: Option<i32>, elapsed: Duration) {
        let facts = self.render.finish(run, &mut self.sink);
        let seconds = elapsed.as_secs_f64();
        if self.quiet && code == 0 && signal.is_none() {
            self.sink.discard();
            let mut facts = facts.join(", ");
            if !facts.is_empty() {
                facts.push_str(", ");
            }
            self.sink.line(&format!(
                "ok  {}: {facts}{seconds:.1} s  [{run}]",
                self.command
            ));
            return;
        }
        self.sink.release();
        let ending = match signal {
            Some(signal) => format!("signal {signal}"),
            None => format!("exit {code}"),
        };
        self.sink.line(&format!(
            "cargo-heli: {run} {ending} in {seconds:.1} s (output: cargo heli log {run})"
        ));
    }

    /// The run's end will not be known (the daemon is lost): prints what
    /// was held back.
    pub fn abandon(mut self, run: &str) {
        self.render.finish(run, &mut self.sink);
        self.sink.release();
    }
}

/// Writes output to one of the client's streams; a reader that has gone
/// away does not stop the run, so errors only cost the output.
pub fn print(out: &mut impl Write, bytes: &[u8]) {
    let written = out.write_all(bytes);
    // A Windows console accepts only UTF-8: show other bytes lossily.
    if matches!(&written, Err(error) if error.kind() == io::ErrorKind::InvalidData) {
        let _ = out.write_all(String::from_utf8_lossy(bytes).as_bytes());
    }
    let _ = out.flush();
}
