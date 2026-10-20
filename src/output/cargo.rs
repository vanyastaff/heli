//! Cargo's diagnostics, compact: the view of build, check, clippy and doc
//! runs in compact and quiet mode (docs/runs.md, "Output").
//!
//! heli adds `--quiet --message-format json` to the command
//! ([`with_json_messages`]), so cargo prints no line per unit and reports
//! each diagnostic as a JSON message on stdout ([`Message`]), and
//! [`Diagnostics`] renders them:
//!
//! - **Errors**, on stderr where cargo would print them, as they arrive: the
//!   first [`FULL_ERRORS`] as rustc renders them, the next [`SHORT_ERRORS`]
//!   on one line each (`path:line:col error[E0308] message`), then, once
//!   the run is over, `N more: cargo heli log <run> --errors` for the rest.
//! - **Warnings**, on stderr once the run is over, as [`Warnings`] says for
//!   the subcommand:
//!   - counted per lint: `12 warnings: 9 unused_imports, 3 dead_code`;
//!   - by lint: `12 warnings:`, then a line per lint with its count and
//!     where its warnings point, `  dead_code (3): src/a.rs:4:5,
//!     src/b.rs:9:1, 1 more`. At most [`WARNING_LOCATIONS`] locations in
//!     all, shared out one per lint in turn, so that every lint shows some.
//!
//!   Lints with the most warnings come first; a warning of no lint is known
//!   by its message.
//! - A diagnostic is shown once, however many targets report it (cargo's
//!   own human output does the same). Notes are dropped.
//! - Cargo's other JSON messages (artifacts, build scripts, the build's
//!   end) are dropped. A line on stdout that is not cargo's JSON, and all of
//!   stderr (cargo's own errors and warnings), pass through; stderr a whole
//!   line at a time ([`Sink::pass`]), so that a rendered error never cuts a
//!   line cargo writes in pieces.
//!
//! [`Message`] and [`Diagnostic`] also serve `cargo heli log --errors`
//! (`crate::runlog::select`), which reads the same JSON from a record.

use std::collections::HashSet;
use std::ffi::OsString;

use serde::Deserialize;

use super::{Render, Sink};
use crate::class::subcommand;
use crate::protocol::Channel;

/// Errors shown as rustc renders them.
const FULL_ERRORS: usize = 3;
/// Errors shown on one line each, after the full ones.
const SHORT_ERRORS: usize = 30;
/// Warning locations shown by lint, at most.
const WARNING_LOCATIONS: usize = 50;

/// How compact output shows warnings, as the run's class says
/// (`super::view_of`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warnings {
    /// A count per lint, on one line: compile runs (build, doc), whose
    /// product is the artifact.
    Counts,
    /// Their locations, grouped by lint: verify runs (check, clippy), where
    /// the findings are the point.
    ByLint,
}

/// Whether cargo reports the diagnostics of `cargo <args>` (`args` from the
/// subcommand on, after a `+toolchain`) as JSON messages and prints nothing
/// else heli would drop: build, check, clippy and doc, and cargo's aliases
/// `b`, `c` and `d`. Of the other compile and verify runs, `run` prints the
/// program's own output and `fmt --check` is rustfmt's; `clippy --fix` is a
/// mutate run.
pub fn reports_json(args: &[OsString]) -> bool {
    subcommand(args)
        .and_then(|(subcommand, _)| subcommand.to_str())
        .is_some_and(|subcommand| {
            matches!(
                subcommand,
                "build" | "b" | "check" | "c" | "clippy" | "doc" | "d"
            )
        })
}

/// Whether `cargo <args>` (`args` from the subcommand on, after a
/// `+toolchain`) already chooses cargo's output: `--message-format`,
/// `--quiet` or `-q`, `--verbose` or `-v`, before any `--`. heli then leaves
/// its output alone, and must: cargo refuses a second `--quiet` or
/// `--message-format`.
///
/// A cluster of short flags counts when it holds a `q` or a `v` anywhere
/// (`-vv`, `-rq`), even in a value (`-pserver`): leaving the output alone
/// is safe, adding `--quiet` to a command that has it is not.
pub fn sets_output(args: &[OsString]) -> bool {
    let Some((_, rest)) = subcommand(args) else {
        return false;
    };
    rest.iter()
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| arg.to_str())
        .any(|arg| match arg.strip_prefix("--") {
            Some(long) => matches!(
                long.split('=').next(),
                Some("message-format" | "quiet" | "verbose")
            ),
            None => arg
                .strip_prefix('-')
                .is_some_and(|cluster| cluster.contains(['q', 'v'])),
        })
}

/// `args` with `--quiet --message-format json` after the user's own
/// arguments and before any `--`, which passes what follows to another
/// program. After them, so that cargo reports a mistake in them exactly as
/// it would without heli (its usage line names the flags it parsed before
/// the mistake).
pub fn with_json_messages(args: &[OsString]) -> Vec<OsString> {
    let at = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut with = args[..at].to_vec();
    with.extend(["--quiet", "--message-format", "json"].map(OsString::from));
    with.extend_from_slice(&args[at..]);
    with
}

/// One line of cargo's JSON output.
pub enum Message {
    /// A diagnostic from rustc, clippy or rustdoc.
    Diagnostic(Diagnostic),
    /// A unit cargo compiled, or found fresh (`compiler-artifact`).
    Artifact { fresh: bool },
    /// The build's end (`build-finished`), and whether it succeeded.
    BuildFinished { success: bool },
    /// Any other message: a build script run.
    Other,
}

impl Message {
    /// The message on `line`, or `None` when it is not one of cargo's JSON
    /// messages.
    pub fn parse(line: &[u8]) -> Option<Self> {
        let line = line.trim_ascii();
        if !line.starts_with(b"{") {
            return None;
        }
        let raw: Raw = serde_json::from_slice(line).ok()?;
        match (raw.reason.as_str(), raw.message) {
            ("compiler-message", Some(message)) => {
                serde_json::from_value(message).ok().map(Self::Diagnostic)
            }
            ("compiler-message", None) => None,
            ("compiler-artifact", _) => Some(Self::Artifact {
                fresh: raw.fresh.unwrap_or(false),
            }),
            ("build-finished", _) => Some(Self::BuildFinished {
                success: raw.success.unwrap_or(false),
            }),
            ("build-script-executed", _) => Some(Self::Other),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct Raw {
    reason: String,
    #[serde(default)]
    message: Option<serde_json::Value>,
    /// Of `compiler-artifact`: the unit was up to date.
    #[serde(default)]
    fresh: Option<bool>,
    /// Of `build-finished`.
    #[serde(default)]
    success: Option<bool>,
}

/// A diagnostic, as cargo relays it from the compiler.
#[derive(Debug, Deserialize)]
pub struct Diagnostic {
    level: String,
    message: String,
    #[serde(default)]
    code: Option<Code>,
    #[serde(default)]
    spans: Vec<Span>,
    #[serde(default)]
    rendered: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Code {
    code: String,
}

#[derive(Debug, Deserialize)]
struct Span {
    file_name: String,
    line_start: u64,
    column_start: u64,
    is_primary: bool,
}

impl Diagnostic {
    /// Whether it is an error (`error`, or `error: internal compiler
    /// error`).
    pub fn is_error(&self) -> bool {
        self.level.starts_with("error")
    }

    /// Whether it is a warning (notes and helps are neither).
    pub fn is_warning(&self) -> bool {
        self.level == "warning"
    }

    /// The diagnostic as rustc renders it, ending a line.
    pub fn rendered(&self) -> String {
        let mut text = self
            .rendered
            .clone()
            .unwrap_or_else(|| format!("{}: {}", self.level, self.message));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }

    /// What tells it apart from other diagnostics: how it is rendered.
    pub fn identity(&self) -> &str {
        self.rendered.as_deref().unwrap_or(&self.message)
    }

    /// Where it points: `path:line:col` of its primary span.
    fn location(&self) -> Option<String> {
        let span = self
            .spans
            .iter()
            .find(|span| span.is_primary)
            .or(self.spans.first())?;
        Some(format!(
            "{}:{}:{}",
            span.file_name, span.line_start, span.column_start
        ))
    }

    /// The lint it comes from (`dead_code`, `clippy::needless_return`); a
    /// warning of no lint is known by its message.
    fn lint(&self) -> String {
        match &self.code {
            Some(code) => code.code.clone(),
            None => self.message.lines().next().unwrap_or_default().to_owned(),
        }
    }

    /// One line: `path:line:col error[code] message`.
    fn short(&self) -> String {
        let mut line = self.location().map(|at| at + " ").unwrap_or_default();
        line.push_str("error");
        if let Some(code) = &self.code {
            line.push_str(&format!("[{}]", code.code));
        }
        let message = self.message.lines().next().unwrap_or_default();
        line.push_str(&format!(" {message}\n"));
        line
    }
}

/// Renders cargo's JSON messages (see the module docs).
pub struct Diagnostics {
    warnings: Warnings,
    /// The start of a line on stdout.
    pending: Vec<u8>,
    /// The diagnostics seen, by [`Diagnostic::identity`].
    seen: HashSet<String>,
    errors: usize,
    /// The warnings, by lint, in the order the lints first came.
    lints: Vec<Lint>,
}

/// The warnings of one lint.
struct Lint {
    name: String,
    count: usize,
    /// Where they point, in the order they came; a warning that points
    /// nowhere has none.
    locations: Vec<String>,
}

impl Diagnostics {
    pub fn new(warnings: Warnings) -> Self {
        Self {
            warnings,
            pending: Vec::new(),
            seen: HashSet::new(),
            errors: 0,
            lints: Vec::new(),
        }
    }

    /// One line of cargo's stdout.
    fn line(&mut self, line: &[u8], out: &mut Sink) {
        if !self.json_line(line, out) {
            out.write(Channel::Stdout, line);
        }
    }

    /// Takes `line` of the run's stdout when it is one of cargo's JSON
    /// messages, and returns whether it was: a view that shows more than
    /// cargo's diagnostics (`super::libtest`) handles the other lines.
    pub fn json_line(&mut self, line: &[u8], out: &mut Sink) -> bool {
        match Message::parse(line) {
            None => return false,
            Some(Message::Artifact { .. } | Message::BuildFinished { .. } | Message::Other) => {}
            Some(Message::Diagnostic(diagnostic)) => self.diagnostic(&diagnostic, out),
        }
        true
    }

    fn diagnostic(&mut self, diagnostic: &Diagnostic, out: &mut Sink) {
        let error = diagnostic.is_error();
        // Notes such as "for more information about this error" go.
        if !error && diagnostic.level != "warning" {
            return;
        }
        if !self.seen.insert(diagnostic.identity().to_owned()) {
            return;
        }
        if !error {
            self.warning(diagnostic);
            return;
        }
        self.errors += 1;
        if self.errors <= FULL_ERRORS {
            out.write(Channel::Stderr, diagnostic.rendered().as_bytes());
        } else if self.errors <= FULL_ERRORS + SHORT_ERRORS {
            out.write(Channel::Stderr, diagnostic.short().as_bytes());
        }
    }

    fn warning(&mut self, diagnostic: &Diagnostic) {
        let name = diagnostic.lint();
        let at = match self.lints.iter().position(|lint| lint.name == name) {
            Some(at) => at,
            None => {
                self.lints.push(Lint {
                    name,
                    count: 0,
                    locations: Vec::new(),
                });
                self.lints.len() - 1
            }
        };
        let lint = &mut self.lints[at];
        lint.count += 1;
        lint.locations.extend(diagnostic.location());
    }

    /// How many warnings there were.
    fn warning_count(&self) -> usize {
        self.lints.iter().map(|lint| lint.count).sum()
    }

    /// The warnings, as [`Warnings`] says, without the last line end; `None`
    /// when there are none.
    fn warnings_summary(&self) -> Option<String> {
        let total = self.warning_count();
        if total == 0 {
            return None;
        }
        let mut lints: Vec<&Lint> = self.lints.iter().collect();
        lints.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        let header = count_of(total, "warning");
        let lines: Vec<String> = match self.warnings {
            Warnings::Counts => {
                let counts: Vec<String> = lints
                    .iter()
                    .map(|lint| format!("{} {}", lint.count, lint.name))
                    .collect();
                vec![format!("{header}: {}", counts.join(", "))]
            }
            Warnings::ByLint => {
                let available: Vec<usize> = lints.iter().map(|lint| lint.locations.len()).collect();
                let shares = share(&available, WARNING_LOCATIONS);
                let groups = lints.iter().zip(shares).map(|(lint, shown)| {
                    let mut line = format!("  {} ({})", lint.name, lint.count);
                    if shown > 0 {
                        line.push_str(": ");
                        line.push_str(&lint.locations[..shown].join(", "));
                        if lint.count > shown {
                            line.push_str(&format!(", {} more", lint.count - shown));
                        }
                    }
                    line
                });
                std::iter::once(format!("{header}:"))
                    .chain(groups)
                    .collect()
            }
        };
        Some(lines.join("\n"))
    }
}

/// Shares `budget` out between groups that have `available` items each:
/// one item per group in turn, so that each group shows some.
fn share(available: &[usize], mut budget: usize) -> Vec<usize> {
    let mut shown = vec![0; available.len()];
    while budget > 0 {
        let mut gave = false;
        for (shown, &available) in shown.iter_mut().zip(available) {
            if budget > 0 && *shown < available {
                *shown += 1;
                budget -= 1;
                gave = true;
            }
        }
        if !gave {
            break;
        }
    }
    shown
}

/// `1 warning`, `3 warnings`.
fn count_of(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

impl Render for Diagnostics {
    fn output(&mut self, channel: Channel, bytes: &[u8], out: &mut Sink) {
        if channel == Channel::Stderr {
            out.pass(bytes);
            return;
        }
        let mut from = self.pending.len();
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending[from..].iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=from + end).collect();
            self.line(&line, out);
            from = 0;
        }
    }

    fn finish(&mut self, run: &str, out: &mut Sink) -> Vec<String> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.line(&line, out);
        }
        let hidden = self.errors.saturating_sub(FULL_ERRORS + SHORT_ERRORS);
        if hidden > 0 {
            out.line(&format!("{hidden} more: cargo heli log {run} --errors"));
        }
        if let Some(summary) = self.warnings_summary() {
            out.line(&summary);
        }
        let warnings = self.warning_count();
        (warnings > 0)
            .then(|| count_of(warnings, "warning"))
            .into_iter()
            .collect()
    }
}
