//! Shared crates (docs/build-store.md#shared-crates, #139): each registry
//! dependency compiled once per host, and kept once, however many worktrees
//! build it.
//!
//! cargo compiles a registry crate with the same arguments in every
//! worktree of a repository: its sources are immutable (cargo verified
//! their checksum), and its `-C metadata` hash does not depend on the
//! workspace's path. Only the build directory in the paths differs. The
//! same holds for vendored and git dependencies: cargo caps the lints of
//! every package that is not a local one (`--cap-lints allow`), and that
//! is how heli tells them. heli sets itself as the `RUSTC_WRAPPER` of the
//! runs it manages (`[store] shared_crates = true`), and for such a
//! compilation:
//!
//! - **the key** is a digest of `rustc -vV`, the arguments with the build
//!   directory's path replaced by a placeholder, the content of each crate
//!   it is compiled against (`--extern`), the package's own `CARGO_*`
//!   variables and `OUT_DIR`, what its build script generated in `OUT_DIR`,
//!   and the variables the build script set for the compiler;
//! - **an entry** in `<store>/.crates/<key>/`, on the build directory's
//!   volume, holds the files rustc wrote (`.rlib`, `.rmeta`, a proc-macro's
//!   library, a build script's program), its dep-info and its messages,
//!   with the build directory replaced by the placeholder, and what else the
//!   compilation read: the variables in its dep-info (`# env-dep:`) and the
//!   files outside the package's own directory, each with its digest;
//! - **a hit** (an entry whose variables and files are as recorded) links
//!   the files into `--out-dir` with hard links, writes the dep-info and
//!   replays the messages with this build directory in them, and compiles
//!   nothing: one physical copy on the volume for every build directory;
//! - **a miss** compiles into a private staging directory, links each file
//!   into `--out-dir` as rustc announces it (cargo starts the dependents on
//!   the `.rmeta`), and publishes the staging directory as the entry by a
//!   rename. A lock per key makes a second compilation of the same crate
//!   wait for the first, and hit.
//!
//! Anything else is compiled as it would be without heli: workspace
//! members, path and git dependencies, incremental compilations, builds
//! outside a store heli manages, and every rustc call without a crate.
//! An existing file at a destination is removed before anything is
//! written there, so that no compilation ever writes through a link into an
//! entry. Any failure of the cache itself compiles as without it.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Set, to `1`, in the environment of a run whose `RUSTC_WRAPPER` is heli:
/// the process cargo starts as the wrapper is this module's.
pub const VAR: &str = "HELI_SHARED_CRATES";

/// The directory of the entries, in a store.
pub const DIR: &str = ".crates";

/// The placeholder for the build directory in what an entry records.
const BUILD: &str = "\u{1}heli-build\u{1}";

/// The version of an entry's layout and key.
const SCHEMA: u32 = 1;

/// Whether this process is heli as cargo's `RUSTC_WRAPPER`: [`VAR`] set and
/// a compiler to run (`rustc`, or `clippy-driver` as cargo's workspace
/// wrapper), not a command of heli's run in a run that has it set.
pub fn wrapping(args: &[OsString]) -> bool {
    std::env::var_os(VAR).is_some_and(|value| value == "1")
        && args.get(1).is_some_and(|program| {
            Path::new(program)
                .file_stem()
                .and_then(OsStr::to_str)
                .is_some_and(|stem| stem.starts_with("rustc") || stem == "clippy-driver")
        })
}

/// The variables that make heli the `RUSTC_WRAPPER` of a run in `cwd`, when
/// `[store] shared_crates` is on and no other wrapper is configured: a
/// variable (`RUSTC_WRAPPER`, `CARGO_BUILD_RUSTC_WRAPPER`, inherited from a
/// parent run too) or `build.rustc-wrapper` in a Cargo configuration that
/// reaches `cwd`, the user's own (sccache) wins.
pub fn environment(
    config: &crate::config::StoreConfig,
    cwd: &Path,
) -> Vec<(&'static str, OsString)> {
    if !config.shared_crates {
        return Vec::new();
    }
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if set("RUSTC_WRAPPER") || set("CARGO_BUILD_RUSTC_WRAPPER") {
        return Vec::new();
    }
    let home = std::env::var_os("CARGO_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")));
    let configured = cwd
        .ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(home)
        .flat_map(|dir| [dir.join("config.toml"), dir.join("config")])
        .filter_map(|file| fs::read_to_string(file).ok())
        .filter_map(|text| text.parse::<toml::Table>().ok())
        .any(|table| {
            table
                .get("build")
                .and_then(toml::Value::as_table)
                .is_some_and(|build| build.contains_key("rustc-wrapper"))
        });
    let Ok(program) = std::env::current_exe() else {
        return Vec::new();
    };
    if configured {
        return Vec::new();
    }
    vec![
        ("RUSTC_WRAPPER", program.into_os_string()),
        (VAR, OsString::from("1")),
    ]
}

/// The entries in `store`: `<store>/.crates/<2 hex>/<hex>`, each with its
/// record.
pub fn entries(store: &Path) -> Vec<PathBuf> {
    let hex = |name: &OsStr, short: bool| {
        name.to_str().is_some_and(|name| {
            (if short {
                name.len() == 2
            } else {
                name.len() > 2
            }) && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    };
    let mut entries = Vec::new();
    let Ok(outer) = fs::read_dir(store.join(DIR)) else {
        return entries;
    };
    for outer in outer.filter_map(Result::ok) {
        if !hex(&outer.file_name(), true) || !outer.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(inner) = fs::read_dir(outer.path()) else {
            continue;
        };
        entries.extend(
            inner
                .filter_map(Result::ok)
                .filter(|entry| {
                    hex(&entry.file_name(), false)
                        && entry.file_type().is_ok_and(|kind| kind.is_dir())
                        && entry.path().join("meta.json").is_file()
                })
                .map(|entry| entry.path()),
        );
    }
    entries.sort();
    entries
}

/// Whether a build directory links a file of the entry `entry`: deleting
/// it would free nothing then.
pub fn linked(entry: &Path) -> bool {
    files_of(entry)
        .iter()
        .any(|(_, meta)| links(&meta.0).is_none_or(|links| links > 1))
}

/// What the entry `entry` holds that no build directory links (the bytes
/// deleting it frees), and when it was last used (its `used` file, which a
/// hit rewrites), in seconds since 1970.
pub fn unshared(entry: &Path) -> (u64, Option<u64>) {
    let mut bytes = 0;
    for (_, (path, len)) in files_of(entry) {
        if links(&path) == Some(1) {
            bytes += len;
        }
    }
    for name in ["meta.json", "used"] {
        bytes += fs::metadata(entry.join(name)).map_or(0, |meta| meta.len());
    }
    let used = modified(&entry.join("used")).map(crate::clock::unix_seconds);
    (bytes, used)
}

/// The files of `entry`, each with its path and length.
fn files_of(entry: &Path) -> Vec<(OsString, (PathBuf, u64))> {
    let Ok(files) = fs::read_dir(entry.join("files")) else {
        return Vec::new();
    };
    files
        .filter_map(Result::ok)
        .filter_map(|file| {
            let meta = file.metadata().ok()?;
            meta.is_file()
                .then(|| (file.file_name(), (file.path(), meta.len())))
        })
        .collect()
}

/// How many names the file at `path` has, if that can be told.
#[cfg(unix)]
fn links(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|meta| meta.nlink())
}

/// How many names the file at `path` has, if that can be told.
#[cfg(windows)]
fn links(path: &Path) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = File::open(path).ok()?;
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is valid; the handle is
    // open for as long as `file` lives.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    (ok != 0).then_some(u64::from(info.nNumberOfLinks))
}

/// Runs the compiler `args[1]` with `args[2..]`, from the shared crates
/// when it can; exits with its status.
pub fn main(args: Vec<OsString>) -> ! {
    let rustc = PathBuf::from(&args[1]);
    let rest: Vec<OsString> = args[2..].to_vec();
    let code = match Compilation::of(&rest) {
        Some(compilation) => compilation.run(&rustc, &rest),
        None => plain(&rustc, &rest),
    };
    std::process::exit(code)
}

/// Runs the compiler as cargo asked; its exit code.
fn plain(rustc: &Path, args: &[OsString]) -> i32 {
    match Command::new(rustc).args(args).status() {
        Ok(status) => code(status),
        Err(error) => {
            eprintln!("cargo-heli: cannot run {}: {error}", rustc.display());
            1
        }
    }
}

fn code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// A compilation of a registry crate in a build directory of a store.
#[derive(Debug)]
struct Compilation {
    crate_name: String,
    extra: String,
    out_dir: PathBuf,
    /// The build directory: `<store>/<2 hex>/<hex>`.
    build: PathBuf,
    /// The store's `.crates/`.
    crates: PathBuf,
    /// The package's directory (`CARGO_MANIFEST_DIR`): immutable sources.
    sources: PathBuf,
}

impl Compilation {
    /// The compilation `args` ask for, when it is one heli shares.
    fn of(args: &[OsString]) -> Option<Self> {
        let sources = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
        Self::of_package(args, sources)
    }

    /// The compilation `args` ask for of the package in `sources`, when it
    /// is one heli shares.
    fn of_package(args: &[OsString], sources: PathBuf) -> Option<Self> {
        let text: Vec<&str> = args.iter().map(|arg| arg.to_str()).collect::<Option<_>>()?;
        let value = |flag: &str| {
            text.iter()
                .position(|arg| *arg == flag)
                .and_then(|at| text.get(at + 1).copied())
        };
        let codegen = |name: &str| {
            let prefix = format!("{name}=");
            text.iter().enumerate().find_map(|(at, arg)| {
                let option = arg
                    .strip_prefix("-C")
                    .filter(|rest| !rest.is_empty())
                    .or_else(|| (*arg == "-C").then(|| text.get(at + 1).copied()).flatten())?;
                option.strip_prefix(prefix.as_str()).map(str::to_owned)
            })
        };
        if codegen("incremental").is_some() || value("--cap-lints") != Some("allow") {
            return None;
        }
        let crate_name = value("--crate-name")?.to_owned();
        let extra = codegen("extra-filename")?;
        let out_dir = PathBuf::from(value("--out-dir")?);
        let source = text
            .iter()
            .find(|arg| arg.ends_with(".rs") && !arg.starts_with('-'))
            .map(Path::new)?;
        // The crate's root is in its package, and the package outside the
        // build directory.
        let build = build_dir(&out_dir)?;
        if !sources.is_absolute() || sources.starts_with(&build) {
            return None;
        }
        if source.is_absolute() && !source.starts_with(&sources) {
            return None;
        }
        let crates = build.parent()?.parent()?.join(DIR);
        Some(Self {
            crate_name,
            extra,
            out_dir,
            build,
            crates,
            sources,
        })
    }

    /// Compiles, or links what an entry holds; the compiler's exit code.
    fn run(&self, rustc: &Path, args: &[OsString]) -> i32 {
        let Some(key) = self.key(rustc, args) else {
            return plain(rustc, args);
        };
        let entry = self.crates.join(&key[..2]).join(&key[2..]);
        if let Some(code) = self.hit(&entry, args) {
            return code;
        }
        // One compilation of a key at a time: a second waits, then hits.
        let lock = fs::create_dir_all(self.crates.join(".locks"))
            .and_then(|()| {
                File::options()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(self.crates.join(".locks").join(&key))
            })
            .and_then(|file| file.lock().map(|()| file));
        if lock.is_ok()
            && let Some(code) = self.hit(&entry, args)
        {
            return code;
        }
        self.miss(rustc, args, &entry)
    }

    /// The key of the compilation `args` with `rustc`, when it can be told.
    fn key(&self, rustc: &Path, args: &[OsString]) -> Option<String> {
        let version = Command::new(rustc).arg("-vV").output().ok()?;
        if !version.status.success() {
            return None;
        }
        let mut hasher = Sha256::new();
        let mut field = |name: &str, value: &[u8]| {
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value);
        };
        field("schema", &SCHEMA.to_le_bytes());
        field("rustc", &version.stdout);
        for arg in args {
            field("arg", self.placeheld(arg.to_str()?).as_bytes());
        }
        // The crates it is compiled against, by content: a dependency
        // compiled here, rather than linked from an entry, is another
        // crate to the compiler.
        for path in externs(args) {
            field("extern", digest_of(&path)?.as_bytes());
        }
        let mut vars: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, _)| {
                name.starts_with("CARGO_PKG_")
                    || name.starts_with("CARGO_MANIFEST_")
                    || name == "CARGO_CRATE_NAME"
                    || name == "OUT_DIR"
            })
            .collect();
        // What the build script set for the compiler (`cargo::rustc-env`).
        if let Some(out) = std::env::var_os("OUT_DIR").map(PathBuf::from) {
            let output = out.parent().map(|dir| dir.join("output"));
            for name in output.map(|output| rustc_env(&output)).unwrap_or_default() {
                if let Ok(value) = std::env::var(&name) {
                    vars.push((name, value));
                }
            }
            for (path, digest) in tree(&out) {
                field("out", self.placeheld(&path).as_bytes());
                field("digest", digest.as_bytes());
            }
        }
        vars.sort();
        vars.dedup();
        for (name, value) in vars {
            field(&name, self.placeheld(&value).as_bytes());
        }
        Some(hex(&hasher.finalize()))
    }

    /// `text` with the build directory replaced by the placeholder.
    fn placeheld(&self, text: &str) -> String {
        text.replace(self.build.to_str().unwrap_or("\u{0}"), BUILD)
    }

    /// `text` with the placeholder replaced by the build directory.
    fn placed(&self, text: &str) -> String {
        text.replace(BUILD, &self.build.to_string_lossy())
    }

    /// Links what `entry` holds into the output directory, when it holds
    /// this compilation still: its variables and files as recorded.
    ///
    /// cargo tells a stale unit by its files' times: a unit whose files are
    /// older than a dependency's is built again. A linked file keeps the
    /// entry's time, which every build directory shares, so it must not be
    /// older than what the compilation depends on here. The build script's
    /// `output`, which cargo writes in every build directory when it runs
    /// the script, gets the entry's time; a dependency (`--extern`) newer
    /// than the entry (built here after the entry was) makes the files
    /// copies of their own, dated now.
    fn hit(&self, entry: &Path, args: &[OsString]) -> Option<i32> {
        let meta: Meta = serde_json::from_slice(&fs::read(entry.join("meta.json")).ok()?).ok()?;
        if meta.schema != SCHEMA || !self.still(&meta) {
            return None;
        }
        let files = entry.join("files");
        let built = meta
            .files
            .iter()
            .filter_map(|name| modified(&files.join(name)))
            .max()?;
        let newer = externs(args)
            .iter()
            .filter_map(|path| modified(path))
            .any(|time| time > built);
        if let Some(output) = std::env::var_os("OUT_DIR")
            .map(PathBuf::from)
            .and_then(|out| out.parent().map(|dir| dir.join("output")))
            && modified(&output).is_some_and(|time| time > built)
        {
            File::options()
                .write(true)
                .open(&output)
                .and_then(|file| file.set_modified(built))
                .ok()?;
        }
        for name in &meta.files {
            let to = self.out_dir.join(name);
            remove(&to);
            if newer {
                fs::copy(files.join(name), &to)
                    .and_then(|_| File::options().write(true).open(&to))
                    .and_then(|file| file.set_modified(std::time::SystemTime::now()))
                    .ok()?;
            } else {
                link(&files.join(name), &to).ok()?;
            }
        }
        if let Some((name, text)) = &meta.dep_info {
            let to = self.out_dir.join(name);
            remove(&to);
            fs::write(&to, self.placed(text)).ok()?;
        }
        let mut stderr = std::io::stderr().lock();
        for line in &meta.messages {
            let _ = writeln!(stderr, "{}", self.placed(line));
        }
        // Its last use, for eviction.
        let _ = File::create(entry.join("used"));
        Some(0)
    }

    /// Whether the variables and files `meta` recorded are as they were.
    fn still(&self, meta: &Meta) -> bool {
        meta.env.iter().all(|(name, value)| {
            std::env::var(name).ok() == value.as_deref().map(|value| self.placed(value))
        }) && meta.read.iter().all(|(path, digest)| {
            digest_of(Path::new(&self.placed(path))).as_deref() == Some(digest.as_str())
        })
    }

    /// Compiles into a staging directory, links its files into the output
    /// directory, and publishes it as `entry`; the compiler's exit code.
    fn miss(&self, rustc: &Path, args: &[OsString], entry: &Path) -> i32 {
        let staging = self.crates.join(".staging").join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos())
        ));
        let files = staging.join("files");
        if fs::create_dir_all(&files).is_err() {
            return plain(rustc, args);
        }
        let mut staged: Vec<OsString> = args.to_vec();
        if let Some(at) = staged.iter().position(|arg| arg == "--out-dir") {
            staged[at + 1] = files.clone().into_os_string();
        }
        for name in self.outputs(&self.out_dir) {
            remove(&self.out_dir.join(name));
        }
        let child = Command::new(rustc)
            .args(&staged)
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                eprintln!("cargo-heli: cannot run {}: {error}", rustc.display());
                let _ = fs::remove_dir_all(&staging);
                return 1;
            }
        };
        let files_text = files.to_string_lossy().into_owned();
        let out_text = self.out_dir.to_string_lossy().into_owned();
        let mut messages = Vec::new();
        let mut linked: Vec<OsString> = Vec::new();
        if let Some(stderr) = child.stderr.take() {
            let mut out = std::io::stderr();
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                // Messages name files where cargo looks for them; JSON
                // escapes a path's backslashes.
                let line = line
                    .replace(&files_text, &out_text)
                    .replace(&json_escaped(&files_text), &json_escaped(&out_text));
                if let Some(name) = artifact(&line)
                    && Path::new(&name).extension() != Some(OsStr::new("d"))
                {
                    // cargo starts the dependents on this: the file must be
                    // there first.
                    let to = self.out_dir.join(&name);
                    remove(&to);
                    if link(&files.join(&name), &to).is_ok() {
                        linked.push(name);
                    }
                }
                let _ = writeln!(out, "{line}");
                messages.push(self.placeheld(&line));
            }
        }
        let status = match child.wait() {
            Ok(status) => status,
            Err(_) => {
                let _ = fs::remove_dir_all(&staging);
                return 1;
            }
        };
        if !status.success() {
            let _ = fs::remove_dir_all(&staging);
            return code(status);
        }
        match self.publish(&staging, &files, &linked, messages, entry) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!(
                    "cargo-heli: cannot keep {} shared: {error}",
                    self.crate_name
                );
                // What rustc wrote and is not in the output directory yet
                // is copied there: the compilation succeeded.
                for name in self.outputs(&files) {
                    let to = self.out_dir.join(&name);
                    if !to.exists() && fs::copy(files.join(&name), &to).is_err() {
                        let _ = fs::remove_dir_all(&staging);
                        return 1;
                    }
                }
                let _ = fs::remove_dir_all(&staging);
                0
            }
        }
    }

    /// Links the staged files that are not yet into the output directory,
    /// writes the dep-info there, records the entry and renames the staging
    /// directory into place.
    fn publish(
        &self,
        staging: &Path,
        files: &Path,
        linked: &[OsString],
        messages: Vec<String>,
        entry: &Path,
    ) -> std::io::Result<()> {
        let mut names = Vec::new();
        let mut dep_info = None;
        for name in self.outputs(files) {
            let from = files.join(&name);
            let to = self.out_dir.join(&name);
            if Path::new(&name).extension() == Some(OsStr::new("d")) {
                let text = fs::read_to_string(&from)?.replace(
                    files.to_string_lossy().as_ref(),
                    self.out_dir.to_string_lossy().as_ref(),
                );
                remove(&to);
                fs::write(&to, &text)?;
                fs::remove_file(&from)?;
                dep_info = Some((name.to_string_lossy().into_owned(), text));
                continue;
            }
            if !linked.contains(&name) {
                remove(&to);
                link(&from, &to)?;
            }
            names.push(name.to_string_lossy().into_owned());
        }
        let (env, read) = match &dep_info {
            Some((_, text)) => self.inputs(text),
            None => (Vec::new(), Vec::new()),
        };
        let meta = Meta {
            schema: SCHEMA,
            crate_name: self.crate_name.clone(),
            files: names,
            dep_info: dep_info.map(|(name, text)| (name, self.placeheld(&text))),
            messages,
            env,
            read,
        };
        fs::write(
            staging.join("meta.json"),
            serde_json::to_vec(&meta).map_err(std::io::Error::other)?,
        )?;
        File::create(staging.join("used"))?;
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::rename(staging, entry).is_err() {
            // Another compilation published it first.
            fs::remove_dir_all(staging)?;
        }
        Ok(())
    }

    /// The variables and files a compilation read, from its dep-info: each
    /// variable with its value (`None`: unset), and each file outside the
    /// registry's sources with its digest, its path placeheld.
    #[allow(clippy::type_complexity)]
    fn inputs(&self, dep_info: &str) -> (Vec<(String, Option<String>)>, Vec<(String, String)>) {
        let mut env = Vec::new();
        let mut read = Vec::new();
        for line in dep_info.lines() {
            if let Some(var) = line.strip_prefix("# env-dep:") {
                let (name, value) = match var.split_once('=') {
                    Some((name, value)) => (name, Some(self.placeheld(&unescape(value)))),
                    None => (var, None),
                };
                env.push((name.to_owned(), value));
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let Some((_, deps)) = split_rule(line) else {
                continue;
            };
            for dep in deps {
                let path = PathBuf::from(&dep);
                if path.starts_with(&self.sources) || path.starts_with(&self.out_dir) {
                    continue;
                }
                let text = dep.clone();
                if read
                    .iter()
                    .any(|(known, _): &(String, String)| *known == text)
                {
                    continue;
                }
                if let Some(digest) = digest_of(&path) {
                    read.push((self.placeheld(&text), digest));
                }
            }
        }
        env.sort();
        env.dedup();
        (env, read)
    }

    /// The files in `dir` this compilation writes: named for the crate and
    /// its extra filename (`libtokio-0123abcd.rlib`, `tokio-0123abcd.d`,
    /// `build_script_build-0123abcd.exe`).
    fn outputs(&self, dir: &Path) -> Vec<OsString> {
        let stem = format!("{}{}", self.crate_name, self.extra);
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<OsString> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| entry.file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                let name = name.strip_prefix("lib").unwrap_or(&name);
                name.strip_prefix(stem.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
            })
            .collect();
        names.sort();
        names
    }
}

/// What an entry records.
#[derive(Serialize, Deserialize)]
struct Meta {
    schema: u32,
    crate_name: String,
    /// The files in `files/`, by name.
    files: Vec<String>,
    /// The dep-info's name and text, placeheld.
    dep_info: Option<(String, String)>,
    /// What the compiler said, a line each, placeheld.
    messages: Vec<String>,
    /// The variables it read, with their values (`None`: unset).
    env: Vec<(String, Option<String>)>,
    /// The files it read outside the registry's sources, placeheld, each
    /// with its digest.
    read: Vec<(String, String)>,
}

/// The build directory `out_dir` is in: its ancestor `<store>/<2 hex>/<hex>`.
fn build_dir(out_dir: &Path) -> Option<PathBuf> {
    let hex = |name: Option<&OsStr>, length: Option<usize>| {
        name.and_then(OsStr::to_str).is_some_and(|name| {
            length.is_none_or(|length| name.len() == length)
                && name.len() >= 2
                && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    };
    out_dir
        .ancestors()
        .find(|dir| {
            hex(dir.file_name(), None)
                && dir.file_name().is_some_and(|name| name.len() > 2)
                && hex(dir.parent().and_then(Path::file_name), Some(2))
        })
        .map(Path::to_path_buf)
}

/// The variables a build script's `output` sets for the compiler.
fn rustc_env(output: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(output) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            line.strip_prefix("cargo::rustc-env=")
                .or_else(|| line.strip_prefix("cargo:rustc-env="))
        })
        .filter_map(|assignment| assignment.split_once('=').map(|(name, _)| name.to_owned()))
        .collect()
}

/// Every file under `dir`, by path, with its digest, sorted.
fn tree(dir: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && let Some(digest) = digest_of(&path)
            {
                files.push((path.to_string_lossy().into_owned(), digest));
            }
        }
    }
    files.sort();
    files
}

/// The SHA-256 of the file at `path`, in hex.
fn digest_of(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `text` as it appears inside a JSON string.
fn json_escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_default()
        .to_owned()
}

/// The file name a rustc artifact notification announces
/// (`{"$message_type":"artifact","artifact":"<path>","emit":"metadata"}`).
fn artifact(line: &str) -> Option<OsString> {
    if !line.starts_with('{') || !line.contains("\"artifact\":") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let path = value.get("artifact")?.as_str()?;
    Path::new(path).file_name().map(OsStr::to_os_string)
}

/// When the file at `path` was last written.
fn modified(path: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// The files of the crates `args` hand the compiler (`--extern
/// name=path`).
fn externs(args: &[OsString]) -> Vec<PathBuf> {
    args.windows(2)
        .filter(|pair| pair[0] == "--extern")
        .filter_map(|pair| {
            pair[1]
                .to_str()?
                .split_once('=')
                .map(|(_, path)| PathBuf::from(path))
        })
        .collect()
}

/// Removes the file at `path`, if there is one: what is written there next
/// never goes through a link into an entry.
fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Links `to` to the file `from` (a hard link: one copy on the volume).
/// A hardlink, or a copy where the volume has none (or the entry is on
/// another one): sharing saves the space only with links, and works without.
fn link(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to).or_else(|_| fs::copy(from, to).map(|_| ()))
}

/// A make rule's targets and prerequisites, unescaped (`\ ` is a space).
fn split_rule(line: &str) -> Option<(String, Vec<String>)> {
    // A Windows path has a `:` after its drive letter: the rule's `:` is
    // the first one followed by a space or the end.
    let bytes = line.as_bytes();
    let colon = (0..bytes.len())
        .find(|&at| bytes[at] == b':' && (at + 1 == bytes.len() || bytes[at + 1] == b' '))?;
    let (targets, rest) = line.split_at(colon);
    let mut deps = Vec::new();
    let mut current = String::new();
    let mut chars = rest[1..].chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            ' ' => {
                if !current.is_empty() {
                    deps.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(char),
        }
    }
    if !current.is_empty() {
        deps.push(current);
    }
    Some((targets.to_owned(), deps))
}

/// A dep-info value as rustc escapes it (`\n`, `\r`, `\\`).
fn unescape(value: &str) -> String {
    let mut text = String::new();
    let mut chars = value.chars();
    while let Some(char) = chars.next() {
        if char != '\\' {
            text.push(char);
            continue;
        }
        match chars.next() {
            Some('n') => text.push('\n'),
            Some('r') => text.push('\r'),
            Some(other) => text.push(other),
            None => text.push('\\'),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &[&str]) -> Vec<OsString> {
        text.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_a_capped_crate_in_a_store_is_shared() {
        let registry = if cfg!(windows) {
            r"C:\home\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f"
        } else {
            "/home/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f"
        };
        let package = Path::new(registry).join("memchr-2.8.3");
        let of = |args: &[OsString]| Compilation::of_package(args, package.clone());
        let build = Path::new(if cfg!(windows) {
            r"D:\heli-cache"
        } else {
            "/heli-cache"
        })
        .join("c2")
        .join("bc1f29c69ee791");
        let source = format!("{registry}/memchr-2.8.3/src/lib.rs");
        let out = build.join("debug").join("deps");
        let out = out.to_str().unwrap();
        let shared = args(&[
            "--crate-name",
            "memchr",
            "--edition=2021",
            &source,
            "--crate-type",
            "lib",
            "-C",
            "metadata=71c5b31944af59c8",
            "-C",
            "extra-filename=-2974315c36bbf455",
            "--out-dir",
            out,
            "--cap-lints",
            "allow",
        ]);
        let compilation = of(&shared).expect("shared");
        assert_eq!(compilation.extra, "-2974315c36bbf455");
        assert_eq!(compilation.build, build);
        assert_eq!(
            compilation.crates,
            build.parent().unwrap().parent().unwrap().join(DIR)
        );
        assert_eq!(compilation.sources, package);
        // A workspace member: lints not capped.
        let mut member = shared.clone();
        member.truncate(member.len() - 2);
        assert!(of(&member).is_none());
        // Incremental.
        let mut incremental = shared.clone();
        incremental.extend(args(&["-C", "incremental=/x"]));
        assert!(of(&incremental).is_none());
        // A build directory outside a store.
        let mut outside = shared.clone();
        let at = outside.iter().position(|arg| arg == "--out-dir").unwrap();
        outside[at + 1] = OsString::from("/work/target/debug/deps");
        assert!(of(&outside).is_none());
        // A crate root outside its package.
        assert!(Compilation::of_package(&shared, build.join("elsewhere")).is_none());
    }

    #[test]
    fn dep_info_rules_split_on_the_rules_colon() {
        let (target, deps) =
            split_rule(r"C:\b\deps\x.d: C:\reg\x\src\lib.rs C:\b\out\gen\ file.rs").unwrap();
        assert_eq!(target, r"C:\b\deps\x.d");
        assert_eq!(deps, [r"C:\reg\x\src\lib.rs", r"C:\b\out\gen file.rs"]);
        assert_eq!(split_rule("/a/x.d:").unwrap().1, Vec::<String>::new());
    }

    #[test]
    fn outputs_are_named_for_the_crate_and_its_hash() {
        let dir = crate::test_tmp::tempdir();
        for name in [
            "libtokio-0123abcd.rlib",
            "libtokio-0123abcd.rmeta",
            "tokio-0123abcd.d",
            "libtokio-0123abcdef.rlib",
            "libtokio_util-0123abcd.rlib",
        ] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        let compilation = Compilation {
            crate_name: "tokio".to_owned(),
            extra: "-0123abcd".to_owned(),
            out_dir: dir.path().to_path_buf(),
            build: dir.path().to_path_buf(),
            crates: dir.path().to_path_buf(),
            sources: dir.path().to_path_buf(),
        };
        assert_eq!(
            compilation.outputs(dir.path()),
            [
                OsString::from("libtokio-0123abcd.rlib"),
                OsString::from("libtokio-0123abcd.rmeta"),
                OsString::from("tokio-0123abcd.d"),
            ]
        );
    }
}
