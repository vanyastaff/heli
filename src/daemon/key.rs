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

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::protocol::RunRequest;
use crate::store::probe::Probe;
use crate::{environment, source, worktree};

/// Raised when what goes into the key, or how, changes, so that markers of
/// another composition never match.
const COMPOSITION: &[u8] = b"3";

/// The local source inventories at one moment: Git trees and raw bytes.
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
    let cwd = request.cwd();
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
    cargo_config(request, &mut fields)?;
    environment(request, &worktree, &mut fields);
    Ok(Some(Key {
        digest: fields.finish(),
        snapshot,
    }))
}

/// The caller and Cargo's local source roots, with raw compiler inputs as
/// well as staged Git identities. Missing inventories disable caching.
pub fn snapshot(request: &RunRequest, scratch: &Path) -> Result<Snapshot, String> {
    let probe = Probe::request(request);
    let selected = probe
        .sources_at(None)
        .ok_or("cannot discover the command's local Cargo sources offline")?;
    let caller = worktree::of(&request.cwd());
    let mut roots = BTreeSet::new();
    roots.insert(caller.clone());
    let mut pending = vec![selected];
    let mut manifests = BTreeSet::new();
    while let Some(workspace) = pending.pop() {
        let root = worktree::of(&workspace.root);
        if !root.join(".git").exists() {
            return Err(format!(
                "{} has no Git source inventory",
                workspace.root.display()
            ));
        }
        roots.insert(root);
        for package in workspace
            .packages
            .iter()
            .filter(|package| package.source.is_none())
        {
            let dir = package
                .manifest_path
                .parent()
                .ok_or("a manifest has no parent")?;
            let root = worktree::of(dir);
            if !root.join(".git").exists() {
                return Err(format!("{} has no Git source inventory", dir.display()));
            }
            roots.insert(root);
            manifests.insert(package.manifest_path.clone());
        }
        for package in workspace
            .packages
            .iter()
            .filter(|package| package.source.is_none())
        {
            for dependency in &package.dependencies {
                if let Some(path) = &dependency.path {
                    let manifest = path.join("Cargo.toml");
                    if manifests.insert(manifest.clone()) {
                        pending.push(probe.sources_at(Some(&manifest)).ok_or_else(|| {
                            format!("cannot discover local dependency {}", path.display())
                        })?);
                    }
                }
            }
        }
    }
    let mut sources = Vec::new();
    for root in roots {
        // A worktree's own absolute path must not prevent reuse by its twin.
        let name = if root.starts_with(&caller) {
            relative(&root, &caller)
        } else {
            root.as_os_str().as_encoded_bytes().to_vec()
        };
        sources.push((name, raw_tree(request, &root, scratch, 0)?));
    }
    sources.sort();
    let mut fields = Fields::default();
    for (name, content) in sources {
        fields.push(b"source-root", &name);
        fields.push(b"source", &content);
    }
    Ok(Snapshot {
        tree: fields.finish().into_bytes(),
    })
}

fn raw_tree(
    request: &RunRequest,
    root: &Path,
    scratch: &Path,
    depth: u32,
) -> Result<Vec<u8>, String> {
    if depth >= 32 {
        return Err("Git submodule nesting exceeds 32".to_owned());
    }
    let git = |args: &[&str], index: Option<&Path>| git_at(request, root, args, index);
    staged(&git, root, scratch, |index| {
        let tree = git(&["write-tree"], Some(index))?;
        let listing = git(&["ls-files", "--stage", "--full-name", "-z"], Some(index))?;
        let mut fields = Fields::default();
        fields.push(b"tree", &trim(tree));
        for entry in listing
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let tab = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or("invalid Git index entry")?;
            let info = &entry[..tab];
            let path = &entry[tab + 1..];
            let file = root.join(git_path(path)?);
            fields.push(b"path", path);
            let identity = if info.starts_with(b"160000 ") {
                raw_tree(request, &file, scratch, depth + 1)?
            } else if info.starts_with(b"120000 ") {
                std::fs::read_link(&file)
                    .map_err(|error| format!("cannot read {}: {error}", file.display()))?
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else {
                source::digest(&file)
                    .map_err(|error| format!("cannot read {}: {error}", file.display()))?
                    .into_bytes()
            };
            fields.push(b"content", &identity);
        }
        Ok(fields.finish().into_bytes())
    })
}

#[cfg(unix)]
fn git_path(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes.to_vec()).into())
}

#[cfg(windows)]
fn git_path(bytes: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| "a Git path is not UTF-8".to_owned())
}

/// Runs git as a caller wants it run: with these arguments, and with this
/// index file instead of the real one when given; returns its stdout.
pub type Git<'a> = dyn Fn(&[&str], Option<&Path>) -> Result<Vec<u8>, String> + 'a;

/// Calls `inspect` with a scratch copy of the index of the worktree that
/// `git` runs in (from `cwd`), in which every change is staged: `git add
/// -A`, untracked files included and ignored ones not. The real index is
/// only read; the copy, in `scratch`, is removed afterwards.
pub fn staged<T>(
    git: &Git<'_>,
    cwd: &Path,
    scratch: &Path,
    inspect: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let index = git(&["rev-parse", "--git-path", "index"], None)?;
    let index = String::from_utf8(index).map_err(|_| "the index's path is not UTF-8".to_owned())?;
    let real = cwd.join(index.trim_end());
    std::fs::create_dir_all(scratch)
        .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
    let copy = scratch.join(format!(
        "index-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let inspected = (|| {
        if real.exists() {
            copy_index(&real, &copy)?;
        }
        git(&["add", "-A"], Some(&copy))?;
        inspect(&copy)
    })();
    let _ = std::fs::remove_file(&copy);
    inspected
}

/// Copies the index `real` to `copy`, with its own time.
///
/// The copy keeps the stat cache, so files unchanged since are not hashed
/// again. git takes a file whose times and size match its entry for
/// unchanged, unless the entry is "racily clean": no older than the index
/// itself, so the file may have changed in the moment the index was written.
/// Where git compares whole seconds (its Linux builds), a file changed to
/// the same size in the second the index was written matches its entry, and
/// only that check finds it. A copy dated later than the index (as
/// `std::fs::copy` dates it on Linux) hides it, and git keeps the entry's
/// old blob. The index's time is read before the copy is made: an index
/// replaced meanwhile only makes more entries racy.
fn copy_index(real: &Path, copy: &Path) -> Result<(), String> {
    let written = std::fs::metadata(real)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| format!("cannot read the index's time: {error}"))?;
    std::fs::copy(real, copy).map_err(|error| format!("cannot copy the index: {error}"))?;
    std::fs::File::options()
        .write(true)
        .open(copy)
        .and_then(|file| file.set_modified(written))
        .map_err(|error| format!("cannot date the copy of the index: {error}"))
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
/// the tool the subcommand runs (clippy, rustfmt, nextest), each asked of
/// the `+toolchain` the run names, if any. A tool that does not answer is
/// keyed as absent.
fn toolchain(request: &RunRequest, fields: &mut Fields) {
    let cargo = request
        .argv
        .first()
        .map_or(OsStr::new("cargo"), |program| &program.0);
    let rustc = request.var("RUSTC").unwrap_or(OsStr::new("rustc"));
    let mut args = request.argv.iter().skip(1).map(|arg| arg.0.as_os_str());
    let mut subcommand = args.next();
    let pinned = subcommand.filter(|first| first.as_encoded_bytes().starts_with(b"+"));
    if pinned.is_some() {
        subcommand = args.next();
    }
    let mut probes: Vec<(&str, &OsStr, &[&str])> =
        vec![("rustc -vV", rustc, &["-vV"]), ("cargo -V", cargo, &["-V"])];
    match subcommand.and_then(OsStr::to_str) {
        Some("clippy") => probes.push(("cargo clippy -V", cargo, &["clippy", "-V"])),
        Some("fmt") => probes.push(("cargo fmt --version", cargo, &["fmt", "--version"])),
        Some("nextest") => {
            probes.push(("cargo nextest --version", cargo, &["nextest", "--version"]));
        }
        _ => {}
    }
    for (name, program, args) in probes {
        let output = tool(
            request,
            program,
            pinned.into_iter().chain(args.iter().map(OsStr::new)),
        )
        .output();
        match output {
            Ok(output) if output.status.success() => {
                fields.push(format!("version {name}").as_bytes(), &output.stdout);
            }
            _ => fields.push(format!("absent {name}").as_bytes(), b""),
        }
    }
}

/// Keys the Cargo configuration cargo reads for the run, by content, in the
/// order cargo reads it: `.cargo/config.toml` and `.cargo/config` in the
/// working directory and every directory above it, nearest first, then in
/// the Cargo home. Their paths are not keyed, so worktrees in different
/// places share a key. A file that is there but cannot be read is keyed by
/// its error, never as absent: cargo may fail on it.
fn cargo_config(request: &RunRequest, fields: &mut Fields) -> Result<(), String> {
    let cwd = request.cwd();
    let dirs = cwd
        .ancestors()
        .map(|dir| ("cargo-config", dir.join(".cargo")))
        .chain(cargo_home(request, &cwd).map(|home| ("cargo-home-config", home)));
    for (place, dir) in dirs {
        for file in ["config.toml", "config"] {
            let path = dir.join(file);
            match std::fs::read(&path) {
                Ok(content) => config_file(
                    &path,
                    &content,
                    format!("{place} {file}").as_bytes(),
                    fields,
                    &mut Vec::new(),
                )?,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => fields.push(
                    format!("unreadable {place} {file}").as_bytes(),
                    format!("{:?} {:?}", error.kind(), error.raw_os_error()).as_bytes(),
                ),
            }
        }
    }
    let mut args = request.argv.iter().skip(1).map(|arg| arg.0.as_os_str());
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let value = if arg == "--config" {
            args.next()
        } else {
            arg.to_str()
                .and_then(|arg| arg.strip_prefix("--config="))
                .map(OsStr::new)
        };
        // Inline KEY=VALUE configuration is already in argv. A file's
        // spelling is there too, but its changing content must count.
        if let Some(value) = value {
            if value.as_encoded_bytes().contains(&b'=') {
                // Cargo also accepts include=[...] through --config. Its
                // external inputs are not covered by keying argv alone.
                config_includes(value.as_encoded_bytes(), &cwd, fields, &mut Vec::new())?;
                continue;
            }
            let path = cwd.join(value);
            match std::fs::read(&path) {
                Ok(content) => config_file(
                    &path,
                    &content,
                    b"cargo-cli-config",
                    fields,
                    &mut Vec::new(),
                )?,
                Err(error) => fields.push(
                    b"unreadable cargo-cli-config",
                    format!("{:?} {:?}", error.kind(), error.raw_os_error()).as_bytes(),
                ),
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ConfigIncludes {
    #[serde(default)]
    include: Vec<ConfigInclude>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ConfigInclude {
    Path(String),
    Options(IncludeOptions),
}

#[derive(Deserialize)]
struct IncludeOptions {
    path: String,
    #[serde(default)]
    optional: bool,
}

/// Hash each include in Cargo's order. Cargo 1.99's per-root seen set
/// treats repeated paths, including diamonds, as include cycles.
fn config_file(
    path: &Path,
    content: &[u8],
    label: &[u8],
    fields: &mut Fields,
    seen: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "cannot identify Cargo configuration {}: {error}",
            path.display()
        )
    })?;
    if seen.len() >= 64 || seen.contains(&canonical) {
        return Err(format!(
            "cyclic or excessively large Cargo config include graph at {}",
            path.display()
        ));
    }
    seen.push(canonical);
    let parent = path.parent().ok_or("a Cargo configuration has no parent")?;
    config_includes(content, parent, fields, seen)?;
    fields.push(label, content);
    Ok(())
}

fn config_includes(
    content: &[u8],
    base: &Path,
    fields: &mut Fields,
    seen: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let text = std::str::from_utf8(content).map_err(|_| "Cargo configuration is not UTF-8")?;
    let config: ConfigIncludes = toml::from_str(text)
        .map_err(|_| "cannot inspect Cargo config includes: invalid configuration")?;
    for include in config.include {
        let (name, optional) = match include {
            ConfigInclude::Path(path) => (path, false),
            ConfigInclude::Options(options) => (options.path, options.optional),
        };
        let joined = base.join(&name);
        if Path::new(&name).extension() != Some(OsStr::new("toml"))
            || name.contains(['*', '?', '[', ']', '{', '}'])
        {
            return Err(format!(
                "invalid Cargo config include path: {}",
                joined.display()
            ));
        }
        // Cargo normalizes `..` lexically before accessing the file. A
        // filesystem traversal through symlinks could read another input.
        let path = normalized_include(&joined)?;
        match std::fs::read(&path) {
            Ok(content) => config_file(&path, &content, b"cargo-include", fields, seen)?,
            Err(error)
                if optional
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
            {
                // Missing optional files are real inputs: creation must
                // invalidate a cached pass, removal must recover absence.
                fields.push(b"missing cargo-include", name.as_bytes());
            }
            Err(error) => {
                return Err(format!(
                    "cannot read Cargo config include {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn normalized_include(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("cannot establish an absolute Cargo config include path".to_owned());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
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

/// `name` as the key holds it: upper-cased on Windows
/// (`environment::folded`).
fn folded(name: &OsStr) -> Vec<u8> {
    environment::folded(name).as_encoded_bytes().to_vec()
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
/// on Windows, where `\` separates too, without case. Elsewhere `\` is a
/// character of a name like any other.
fn spelled(path: &OsStr) -> Vec<u8> {
    let mut bytes: Vec<u8> = path
        .as_encoded_bytes()
        .iter()
        .map(|&byte| {
            if cfg!(windows) && byte == b'\\' {
                b'/'
            } else {
                byte
            }
        })
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
/// - shell bookkeeping (`environment::SHELL_BOOKKEEPING`);
/// - per-session identifiers of terminals and agent harnesses.
fn neutral(name: &[u8], value: &OsStr) -> bool {
    let Ok(name) = std::str::from_utf8(name) else {
        return false;
    };
    if let Some(valid) = valid_count(name, value) {
        return valid;
    }
    const NAMES: [&str; 21] = [
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
    NAMES.contains(&name)
        || environment::SHELL_BOOKKEEPING.contains(&name)
        || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
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
    if let Some(home) = request.var("CARGO_HOME").filter(|home| !home.is_empty()) {
        return Some(cwd.join(home));
    }
    let user = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    request
        .var(user)
        .filter(|home| !home.is_empty())
        .map(|home| Path::new(home).join(".cargo"))
}

/// `git <args>` as the client would run it, with `index` as the index when
/// given; its stdout. Never takes the optional locks that would refresh the
/// real index.
fn git_at(
    request: &RunRequest,
    cwd: &Path,
    args: &[&str],
    index: Option<&Path>,
) -> Result<Vec<u8>, String> {
    let mut command = tool(request, OsStr::new("git"), args);
    command.current_dir(cwd);
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
fn tool<A: AsRef<OsStr>>(
    request: &RunRequest,
    program: &OsStr,
    args: impl IntoIterator<Item = A>,
) -> Command {
    let env = request.env.iter().map(|(name, value)| (&name.0, &value.0));
    environment::command(program, args, &request.cwd(), env)
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
