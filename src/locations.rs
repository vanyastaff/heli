//! Where heli keeps its per-user state, and where its daemon listens.
//!
//! - **State directory**: [`STATE_DIR_VAR`] if set, else
//!   `%LOCALAPPDATA%\heli` on Windows and `$XDG_STATE_HOME/heli` (else
//!   `~/.local/state/heli`) elsewhere. It holds the daemon's lock, its log,
//!   the run counter and the run log. On Unix heli creates it with mode 0700.
//! - **Endpoint**: [`ENDPOINT_VAR`] if set, else derived from the state
//!   directory. On Windows it is the named pipe
//!   `\\.\pipe\heli-<hash of the state directory>-<logon session>`, so each
//!   logon session of a user has a daemon of its own; elsewhere it is the
//!   socket `<state directory>/daemon.sock`.
//! - **Host pool** (`crate::pool`): [`POOL_VAR`] if set, else on Windows the
//!   semaphore `heli-<hash of the state directory>` (semaphore names are
//!   already per logon session), and elsewhere the FIFO
//!   `$XDG_RUNTIME_DIR/heli/jobserver`, else `~/.cache/heli/jobserver`.
//! - **Configuration file** (`crate::config`): [`CONFIG_VAR`] if set, else
//!   `%APPDATA%\heli\config.toml` on Windows and
//!   `$XDG_CONFIG_HOME/heli/config.toml` (else
//!   `~/.config/heli/config.toml`) elsewhere.
//!
//! Tests set these variables to temporary paths and names, so their daemons
//! and pools never meet each other or the user's real ones.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Selects the state directory.
pub const STATE_DIR_VAR: &str = "HELI_STATE_DIR";
/// Selects the daemon's endpoint.
pub const ENDPOINT_VAR: &str = "HELI_ENDPOINT";
/// Selects the host pool: a semaphore name on Windows, a FIFO path
/// elsewhere.
pub const POOL_VAR: &str = "HELI_POOL";
/// Selects the configuration file.
pub const CONFIG_VAR: &str = "HELI_CONFIG";

/// The resolved locations; paths are absolute.
#[derive(Debug, Clone)]
pub struct Locations {
    pub state_dir: PathBuf,
    pub endpoint: PathBuf,
    /// The host pool: a semaphore name on Windows, a FIFO path elsewhere.
    pub pool: OsString,
    pub config: PathBuf,
}

impl Locations {
    /// Resolves the locations from this process's environment; the error
    /// says what is missing.
    pub fn resolve() -> Result<Self, String> {
        let state_dir = match var(STATE_DIR_VAR) {
            Some(dir) => absolute(PathBuf::from(dir))?,
            None => default_state_dir()?,
        };
        let endpoint = match var(ENDPOINT_VAR) {
            Some(endpoint) => absolute(PathBuf::from(endpoint))?,
            None => default_endpoint(&state_dir),
        };
        let pool = match var(POOL_VAR) {
            Some(pool) if cfg!(windows) => pool,
            Some(pool) => absolute(PathBuf::from(pool))?.into(),
            None => default_pool(&state_dir)?,
        };
        let config = match var(CONFIG_VAR) {
            Some(config) => absolute(PathBuf::from(config))?,
            None => default_config()?,
        };
        Ok(Self {
            state_dir,
            endpoint,
            pool,
            config,
        })
    }

    /// The lock the daemon holds while it runs: one daemon per user (and,
    /// on Windows, per logon session).
    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join(session_suffixed("daemon", ".lock"))
    }

    /// The daemon's own log: its start, its end and its errors.
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join(session_suffixed("daemon", ".log"))
    }

    /// The next run number, shared by every daemon of this state directory
    /// so run ids stay unique (`daemon::registry::RunIds`).
    pub fn run_counter(&self) -> PathBuf {
        self.state_dir.join("next-run")
    }

    /// The lock held while a run number is taken from [`Self::run_counter`].
    pub fn run_counter_lock(&self) -> PathBuf {
        self.state_dir.join("next-run.lock")
    }

    /// The run log: one record of output per run (`crate::runlog`).
    pub fn run_logs(&self) -> PathBuf {
        self.state_dir.join("runs")
    }

    /// Creates the state directory, owner-only on Unix.
    pub fn create_state_dir(&self) -> std::io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&self.state_dir)
    }
}

/// A variable's value, unless it is unset or empty.
fn var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    std::path::absolute(&path)
        .map_err(|error| format!("cannot make {} absolute: {error}", path.display()))
}

#[cfg(windows)]
fn default_state_dir() -> Result<PathBuf, String> {
    var("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join("heli"))
        .ok_or_else(|| format!("neither {STATE_DIR_VAR} nor LOCALAPPDATA is set"))
}

#[cfg(unix)]
fn default_state_dir() -> Result<PathBuf, String> {
    let state_home = var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".local").join("state")));
    state_home
        .map(|dir| dir.join("heli"))
        .ok_or_else(|| format!("neither {STATE_DIR_VAR} nor HOME is set"))
}

#[cfg(windows)]
fn default_endpoint(state_dir: &Path) -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\heli-{:016x}-{}",
        path_hash(state_dir),
        session_id()
    ))
}

#[cfg(unix)]
fn default_endpoint(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.sock")
}

/// The semaphore `heli-<hash of the state directory>`: one per user, in the
/// logon session's namespace.
#[cfg(windows)]
fn default_pool(state_dir: &Path) -> Result<OsString, String> {
    Ok(format!("heli-{:016x}", path_hash(state_dir)).into())
}

/// The FIFO `$XDG_RUNTIME_DIR/heli/jobserver`, else
/// `~/.cache/heli/jobserver`.
#[cfg(unix)]
fn default_pool(_state_dir: &Path) -> Result<OsString, String> {
    let dir = var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".cache")));
    dir.map(|dir| dir.join("heli").join("jobserver").into())
        .ok_or_else(|| format!("neither {POOL_VAR}, XDG_RUNTIME_DIR nor HOME is set"))
}

#[cfg(windows)]
fn default_config() -> Result<PathBuf, String> {
    var("APPDATA")
        .map(|dir| PathBuf::from(dir).join("heli").join("config.toml"))
        .ok_or_else(|| format!("neither {CONFIG_VAR} nor APPDATA is set"))
}

#[cfg(unix)]
fn default_config() -> Result<PathBuf, String> {
    let config_home = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".config")));
    config_home
        .map(|dir| dir.join("heli").join("config.toml"))
        .ok_or_else(|| format!("neither {CONFIG_VAR} nor HOME is set"))
}

/// The hash of `path` that names what belongs to its state directory.
#[cfg(windows)]
fn path_hash(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;

    let bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    fnv1a(&bytes)
}

/// `<stem>-<logon session><extension>`.
#[cfg(windows)]
fn session_suffixed(stem: &str, extension: &str) -> String {
    format!("{stem}-{}{extension}", session_id())
}

/// `<stem><extension>`.
#[cfg(not(windows))]
fn session_suffixed(stem: &str, extension: &str) -> String {
    format!("{stem}{extension}")
}

/// This process's logon session (Terminal Services session id).
#[cfg(windows)]
pub fn session_id() -> u32 {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let mut session = 0;
    // SAFETY: queries this process; on failure `session` stays 0, the
    // console session, which only merges this session's daemon with it.
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
    }
    session
}

/// FNV-1a, 64 bits: a hash that stays the same across builds, so clients and
/// daemons of different versions derive the same endpoint and can tell each
/// other their protocol versions.
#[cfg(windows)]
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}
