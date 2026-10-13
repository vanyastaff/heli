//! A run's key in the result store (DESIGN.md §7): a digest of everything
//! that decides the verdict of a verify or test run.
//!
//! **Encoding.** Every input is a field, fed to SHA-256 as its name's length,
//! the name, its value's length and the value, so no two lists of fields
//! hash the same bytes. Values are raw bytes as the platform holds them,
//! never lossy UTF-8.
//!
//! **Inputs**, each read with the client's environment in the client's
//! working directory:
//!
//! - the tree: the worktree with every change staged, untracked files
//!   included and ignored ones not ([`snapshot`]);
//! - the argv, normalized: without the program, which the toolchain stands
//!   for, and with the working directory relative to the worktree;
//! - the machine: its name, OS and architecture, so a state directory in a
//!   shared home never certifies another machine;
//! - the toolchain ([`toolchain`]): `rustc -vV`, and the versions of cargo
//!   and of the tool the run uses, a missing one keyed as absent;
//! - the Cargo configuration ([`cargo_config`]): every `config.toml` and
//!   `config` cargo reads, by content;
//! - the environment ([`environment`]): every variable but the neutral ones.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::daemon::execute;
use crate::protocol::RunRequest;
use crate::worktree;

/// Raised when what goes into the key, or how, changes, so that markers of
/// another composition never match.
const COMPOSITION: &[u8] = b"1";

/// The worktree's content at one moment: its tree with every change staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    tree: Vec<u8>,
}

/// A run's key: the digest that names its marker, and the snapshot it was
/// computed from.
#[derive(Debug, Clone)]
pub struct Key {
    pub digest: String,
    pub snapshot: Snapshot,
}

/// The key of `request` as its worktree is now; `Ok(None)` outside a git
/// repository, where runs are not keyed. `scratch` is a directory for
/// scratch files.
pub fn compute(request: &RunRequest, scratch: &Path) -> Result<Option<Key>, String> {
    let cwd = execute::cwd(request);
    let worktree = worktree::of(&cwd);
    if !worktree.join(".git").exists() {
        return Ok(None);
    }
    let snapshot = snapshot(request, scratch)?;
    let mut fields = Fields::default();
    fields.push(b"composition", COMPOSITION);
    fields.push(b"tree", &snapshot.tree);
    fields.push(b"dir", &relative(&cwd, &worktree));
    for arg in request.argv.iter().skip(1) {
        fields.push(b"arg", arg.0.as_encoded_bytes());
    }
    fields.push(b"machine", &machine());
    toolchain(request, &mut fields);
    cargo_config(request, &mut fields);
    environment(request, &worktree, &mut fields);
    Ok(Some(Key {
        digest: fields.finish(),
        snapshot,
    }))
}

/// The tree of `request`'s worktree with every change staged: `git add -A`
/// into a scratch copy of the index, then `git write-tree`. The real index is
/// only read.
pub fn snapshot(request: &RunRequest, scratch: &Path) -> Result<Snapshot, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let index = git(request, &["rev-parse", "--git-path", "index"], None)?;
    let index = String::from_utf8(index).map_err(|_| "the index's path is not UTF-8".to_owned())?;
    let real = execute::cwd(request).join(index.trim_end());
    std::fs::create_dir_all(scratch)
        .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
    let copy = scratch.join(format!(
        "index-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let tree = (|| {
        if real.exists() {
            // The copy keeps the stat cache, so unchanged files are not
            // hashed again.
            std::fs::copy(&real, &copy)
                .map_err(|error| format!("cannot copy the index: {error}"))?;
        }
        git(request, &["add", "-A"], Some(&copy))?;
        git(request, &["write-tree"], Some(&copy))
    })();
    let _ = std::fs::remove_file(&copy);
    Ok(Snapshot { tree: trim(tree?) })
}

/// This machine: its name (lower-cased), OS and architecture.
fn machine() -> Vec<u8> {
    let mut name = host_name();
    name.make_ascii_lowercase();
    name.extend_from_slice(
        format!(" {} {}", std::env::consts::OS, std::env::consts::ARCH).as_bytes(),
    );
    name
}

/// The host name, empty if it cannot be read.
#[cfg(unix)]
fn host_name() -> Vec<u8> {
    unsafe extern "C" {
        fn gethostname(name: *mut std::ffi::c_char, length: usize) -> std::ffi::c_int;
    }
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer and its length match; the name may lack its NUL
    // when truncated, which the search below allows for.
    let ok = unsafe { gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    if ok {
        buffer[..end].to_vec()
    } else {
        Vec::new()
    }
}

/// The host name, empty if it cannot be read.
#[cfg(windows)]
fn host_name() -> Vec<u8> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetComputerNameW(buffer: *mut u16, size: *mut u32) -> i32;
    }
    let mut buffer = [0u16; 256];
    let mut size = buffer.len() as u32;
    // SAFETY: `size` is the buffer's length in characters, as required.
    let ok = unsafe { GetComputerNameW(buffer.as_mut_ptr(), &mut size) } != 0;
    if ok {
        String::from_utf16_lossy(&buffer[..size as usize]).into_bytes()
    } else {
        Vec::new()
    }
}

/// Keys the toolchain as the client's worktree selects it: `rustc -vV`
/// (`RUSTC` if set), `cargo -V` (the run's own cargo), and the version of
/// the tool the subcommand runs (clippy, rustfmt, nextest). A tool that does
/// not answer is keyed as absent.
fn toolchain(request: &RunRequest, fields: &mut Fields) {
    let cargo = request
        .argv
        .first()
        .map_or(OsStr::new("cargo"), |program| &program.0);
    let rustc = var(request, "RUSTC").unwrap_or(OsStr::new("rustc"));
    let mut probes: Vec<(&str, &OsStr, &[&str])> =
        vec![("rustc -vV", rustc, &["-vV"]), ("cargo -V", cargo, &["-V"])];
    let subcommand = request.argv.get(1).and_then(|arg| arg.0.to_str());
    match subcommand {
        Some("clippy") => probes.push(("cargo clippy -V", cargo, &["clippy", "-V"])),
        Some("fmt") => probes.push(("cargo fmt --version", cargo, &["fmt", "--version"])),
        Some("nextest") => {
            probes.push(("cargo nextest --version", cargo, &["nextest", "--version"]));
        }
        _ => {}
    }
    for (name, program, args) in probes {
        let output = tool(request, program, args).output();
        match output {
            Ok(output) if output.status.success() => {
                fields.push(format!("version {name}").as_bytes(), &output.stdout);
            }
            _ => fields.push(format!("absent {name}").as_bytes(), b""),
        }
    }
}

/// Keys the Cargo configuration cargo reads for the run, by content and
/// place: `.cargo/config.toml` and `.cargo/config` in the working directory
/// and every directory above it (by how far above), then in the Cargo home.
fn cargo_config(request: &RunRequest, fields: &mut Fields) {
    let cwd = execute::cwd(request);
    let dirs = cwd
        .ancestors()
        .enumerate()
        .map(|(up, dir)| (up.to_string(), dir.join(".cargo")))
        .chain(cargo_home(request, &cwd).map(|home| ("home".to_owned(), home)));
    for (place, dir) in dirs {
        for file in ["config.toml", "config"] {
            if let Ok(content) = std::fs::read(dir.join(file)) {
                fields.push(format!("cargo-config {place}/{file}").as_bytes(), &content);
            }
        }
    }
}

/// Keys the client's environment: every variable but the [`neutral`] ones,
/// sorted, each a field of its own. Names are case-folded on Windows only,
/// where they ignore case. The search paths ([`SEARCH_PATHS`]) are keyed
/// without their entries inside `worktree`, which cargo prepends for the
/// programs it runs; one left without entries counts as unset.
fn environment(request: &RunRequest, worktree: &Path, fields: &mut Fields) {
    let mut env: Vec<(Vec<u8>, Vec<u8>)> = request
        .env
        .iter()
        .map(|(name, value)| (folded(&name.0), &value.0))
        .filter(|(name, value)| !neutral(name, value))
        .filter_map(|(name, value)| {
            if SEARCH_PATHS.iter().any(|path| name == path.as_bytes()) {
                let entries = outside(value, worktree);
                (!entries.is_empty()).then_some((name, entries))
            } else {
                Some((name, value.as_encoded_bytes().to_vec()))
            }
        })
        .collect();
    env.sort();
    for (name, value) in env {
        let mut field = b"env ".to_vec();
        field.extend_from_slice(&name);
        fields.push(&field, &value);
    }
}

/// `name` as the key holds it: upper-cased on Windows.
fn folded(name: &OsStr) -> Vec<u8> {
    let mut name = name.as_encoded_bytes().to_vec();
    if cfg!(windows) {
        name.make_ascii_uppercase();
    }
    name
}

/// The program and library search paths.
const SEARCH_PATHS: [&str; 6] = [
    "PATH",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_FRAMEWORK_PATH",
];

/// The entries of the search path `value` outside `worktree`, each
/// length-prefixed, as the platform encodes them.
fn outside(value: &OsStr, worktree: &Path) -> Vec<u8> {
    let root = spelled(worktree.as_os_str());
    let mut kept = Vec::new();
    for entry in std::env::split_paths(value) {
        let spelling = spelled(entry.as_os_str());
        let inside = spelling
            .strip_prefix(root.as_slice())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"));
        if !inside {
            let bytes = entry.as_os_str().as_encoded_bytes();
            kept.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            kept.extend_from_slice(bytes);
        }
    }
    kept
}

/// `path` for comparing places: `/` as the separator, no trailing one, and
/// on Windows without case.
fn spelled(path: &OsStr) -> Vec<u8> {
    let mut bytes: Vec<u8> = path
        .as_encoded_bytes()
        .iter()
        .map(|&byte| if byte == b'\\' { b'/' } else { byte })
        .collect();
    while bytes.len() > 1 && bytes.last() == Some(&b'/') {
        bytes.pop();
    }
    if cfg!(windows) {
        bytes.make_ascii_lowercase();
    }
    bytes
}

/// Whether the variable `name` (as [`folded`]) set to `value` is proven not
/// to change a run's verdict. Everything else is keyed, so a variable
/// nobody has looked at, a test control such as `PROPTEST_CASES` among
/// them, misses the store instead of reusing a verdict it may not share.
/// Neutral:
///
/// - job and thread counts holding a value the tool accepts ([`valid_count`]);
///   a value it rejects fails the run, so it is keyed;
/// - `CARGO_MAKEFLAGS`, the jobserver heli itself hands every run;
/// - heli's own variables (`HELI_*`);
/// - cargo's terminal styling (`CARGO_TERM_*`);
/// - what cargo sets for the program it runs (`CARGO`, `CARGO_MANIFEST_DIR`,
///   `CARGO_PKG_*` and the like), for heli run from a cargo-run program;
/// - locations whose content is keyed elsewhere: `CARGO_HOME` (its
///   configuration), `CARGO_TARGET_DIR` (the outputs) and `RUSTUP_*` (the
///   toolchain they select, keyed by its versions);
/// - shell bookkeeping (`PWD`, `OLDPWD`, `SHLVL`, `_`);
/// - per-session identifiers of terminals and agent harnesses.
fn neutral(name: &[u8], value: &OsStr) -> bool {
    let Ok(name) = std::str::from_utf8(name) else {
        return false;
    };
    if let Some(valid) = valid_count(name, value) {
        return valid;
    }
    const NAMES: [&str; 25] = [
        "CARGO_MAKEFLAGS",
        "CARGO",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_CRATE_NAME",
        "CARGO_BIN_NAME",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_RUSTC_CURRENT_DIR",
        "CARGO_TARGET_TMPDIR",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
        "WT_SESSION",
        "WT_PROFILE_ID",
        "TERM_SESSION_ID",
        "ITERM_SESSION_ID",
        "TMUX_PANE",
        "WINDOWID",
        "XDG_SESSION_ID",
        "SECURITYSESSIONID",
        "CLAUDECODE",
        "CHROME_CRASHPAD_PIPE_NAME",
    ];
    const PREFIXES: [&str; 6] = [
        "HELI_",
        "CARGO_TERM_",
        "CARGO_PKG_",
        "CARGO_BIN_EXE_",
        "RUSTUP_",
        "CLAUDE_",
    ];
    NAMES.contains(&name) || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// For a job or thread count variable, whether `value` is a count its tool
/// accepts; `None` for any other variable.
///
/// - `CARGO_BUILD_JOBS` (cargo's `build.jobs`): a non-zero integer, negative
///   counting back from the CPUs, or `default`;
/// - `NEXTEST_TEST_THREADS` (`--test-threads`): a non-zero integer, negative
///   counting back from the CPUs, or `num-cpus`;
/// - `RUST_TEST_THREADS` (libtest) and `NUMBER_OF_PROCESSORS` (Windows' CPU
///   count): a positive integer.
fn valid_count(name: &str, value: &OsStr) -> Option<bool> {
    let value = value.to_str().unwrap_or("");
    let non_zero = || value.parse::<i64>().is_ok_and(|count| count != 0);
    let positive = || value.parse::<u64>().is_ok_and(|count| count > 0);
    match name {
        "CARGO_BUILD_JOBS" => Some(value == "default" || non_zero()),
        "NEXTEST_TEST_THREADS" => Some(value == "num-cpus" || non_zero()),
        "RUST_TEST_THREADS" | "NUMBER_OF_PROCESSORS" => Some(positive()),
        _ => None,
    }
}

/// The Cargo home as cargo finds it for the run: `CARGO_HOME` (relative to
/// the working directory), else `.cargo` in the user's home.
fn cargo_home(request: &RunRequest, cwd: &Path) -> Option<PathBuf> {
    if let Some(home) = var(request, "CARGO_HOME").filter(|home| !home.is_empty()) {
        return Some(cwd.join(home));
    }
    let user = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    var(request, user)
        .filter(|home| !home.is_empty())
        .map(|home| Path::new(home).join(".cargo"))
}

/// The client's variable `name` (ignoring case on Windows, as there).
fn var<'a>(request: &'a RunRequest, name: &str) -> Option<&'a OsStr> {
    request
        .env
        .iter()
        .find(|(candidate, _)| same_name(&candidate.0, name))
        .map(|(_, value)| value.0.as_os_str())
}

#[cfg(windows)]
fn same_name(name: &OsString, other: &str) -> bool {
    name.eq_ignore_ascii_case(other)
}

#[cfg(not(windows))]
fn same_name(name: &OsString, other: &str) -> bool {
    name == other
}

/// `git <args>` as the client would run it, with `index` as the index when
/// given; its stdout. Never takes the optional locks that would refresh the
/// real index.
fn git(request: &RunRequest, args: &[&str], index: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut command = tool(request, OsStr::new("git"), args);
    command.env("GIT_OPTIONAL_LOCKS", "0");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// `program args` in the client's working directory with exactly the
/// client's environment, with no input and its stdout captured.
fn tool(request: &RunRequest, program: &OsStr, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(execute::cwd(request))
        .env_clear()
        .envs(request.env.iter().map(|(name, value)| (&name.0, &value.0)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// `bytes` without trailing whitespace.
fn trim(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes.pop();
    }
    bytes
}

/// `dir` relative to `worktree` (which contains it), its components joined
/// with `/`.
fn relative(dir: &Path, worktree: &Path) -> Vec<u8> {
    let inside = dir.strip_prefix(worktree).unwrap_or(Path::new(""));
    let parts: Vec<&OsStr> = inside.iter().collect();
    parts
        .iter()
        .map(|part| part.as_encoded_bytes())
        .collect::<Vec<_>>()
        .join(&b'/')
}

/// The fields of a key, hashed as they come.
#[derive(Default)]
struct Fields {
    hasher: Sha256,
}

impl Fields {
    fn push(&mut self, name: &[u8], value: &[u8]) {
        for part in [name, value] {
            self.hasher.update((part.len() as u64).to_le_bytes());
            self.hasher.update(part);
        }
    }

    /// The digest, in hex.
    fn finish(self) -> String {
        self.hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}
