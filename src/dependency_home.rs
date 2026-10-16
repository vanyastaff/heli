//! Private Cargo homes for an explicitly selected immutable dependency epoch.
//!
//! The caller verifies that the selected Cargo supports config includes
//! (the supported profile is Cargo 1.99+). Includes keep the original home
//! configuration at home precedence and preserve its relative-path origin.
//! Generated homes are published once, never rewritten under another run.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dependencies::Epoch;

const MARKER: &str = ".heli-private-home.json";
const CREDENTIALS: [&str; 2] = ["credentials", "credentials.toml"];

pub struct Prepared {
    /// Keeps the selected source epoch leased until the run is handed off.
    pub epoch: Epoch,
    pub home: PathBuf,
    /// The caller preserves Cargo's external-tool search using this bin dir.
    pub original_home: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct Marker {
    schema: u32,
    original_home: PathBuf,
    epoch_path: PathBuf,
    epoch_identity: String,
    config_digest: String,
}

/// Prepare with the invoking client's environment, before replacing Cargo
/// home. `home_base` belongs to heli's private state, outside epoch GC.
pub fn prepare(epoch_path: &Path, home_base: &Path) -> Result<Prepared, String> {
    let epoch = Epoch::open(epoch_path)?;
    let home_base = absolute(home_base)?;
    let cwd = std::env::current_dir().map_err(|error| format!("cannot read cwd: {error}"))?;
    let current = current_home(&cwd)?;
    let marker = read_marker(&current)?;
    // Nested heli invocations retain their parent's exact home, even from
    // a generated workspace or after the original credentials are rotated.
    if let Some(marker) = &marker {
        validate_home(&current, marker)?;
        if marker.epoch_path == epoch.path() && marker.epoch_identity == epoch.identity() {
            return Ok(Prepared {
                original_home: marker.original_home.clone(),
                home: current,
                epoch,
            });
        }
    }
    let original = marker.map_or(current, |marker| marker.original_home);
    let workspace = absolute(&crate::worktree::of(&cwd))?;
    let home = publish(
        &original,
        &workspace,
        epoch.path(),
        epoch.identity(),
        epoch.config(),
        &home_base,
    )?;
    Ok(Prepared {
        epoch,
        home,
        original_home: original,
    })
}

fn current_home(cwd: &Path) -> Result<PathBuf, String> {
    let home = std::env::var_os("CARGO_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            std::env::var_os(name)
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cargo"))
        })
        .ok_or("cannot locate the original Cargo home; set CARGO_HOME")?;
    std::path::absolute(cwd.join(home))
        .map(|path| plain_path(&path))
        .map_err(|error| format!("cannot resolve Cargo home: {error}"))
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    let path = std::path::absolute(path)
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    Ok(crate::store::simplified(
        &fs::canonicalize(&path).unwrap_or(path),
    ))
}

fn read_marker(home: &Path) -> Result<Option<Marker>, String> {
    let path = home.join(MARKER);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot inspect private Cargo home metadata: {error}"
            ));
        }
        Ok(_) => {}
    }
    // Do not trust a forged marker in a shared/writable home to redirect
    // the original credential/config selection, even when switching epochs.
    check_private(home)?;
    check_private(&path)?;
    let bytes = fs::read(&path)
        .map_err(|error| format!("cannot read private Cargo home metadata: {error}"))?;
    let marker: Marker = serde_json::from_slice(&bytes)
        .map_err(|_| format!("invalid private Cargo home metadata in {}", home.display()))?;
    if marker.schema != 1 || !marker.original_home.is_absolute() {
        return Err(format!(
            "unsupported private Cargo home metadata in {}",
            home.display()
        ));
    }
    Ok(Some(marker))
}

fn optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
        Ok(_) => fs::read(path)
            .map(Some)
            .map_err(|error| format!("cannot read {}: {error}", path.display())),
    }
}

fn config(original: &Path, epoch_config: &Path) -> Result<(String, Option<Vec<u8>>), String> {
    // Cargo prefers legacy config over config.toml, but includes accept
    // only .toml. Copying/rebasing a legacy file would change path origins.
    match fs::symlink_metadata(original.join("config")) {
        Ok(_) => {
            return Err(format!(
                "dependency epochs cannot preserve legacy Cargo home config {}; \
                 migrate it to config.toml (keeping its directory) before opting in",
                original.join("config").display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot inspect Cargo home config: {error}")),
    }
    let original_config = original.join("config.toml");
    let original_bytes = optional(&original_config)?;
    let mut included = Vec::new();
    if original_bytes.is_some() {
        included.push(path_text(&original_config)?);
    }
    included.push(path_text(epoch_config)?);
    let array = serde_json::to_string(&included).expect("paths serialize");
    Ok((format!("include = {array}\n"), original_bytes))
}

fn path_text(path: &Path) -> Result<String, String> {
    plain_path(path)
        .into_os_string()
        .into_string()
        .map_err(|_| "dependency epoch configuration paths must be valid Unicode".to_owned())
}

/// Strip only a verbatim prefix: canonicalizing a symlinked config file or
/// Cargo home would relocate its relative configuration. Cargo includes
/// treat the question mark in that Windows prefix as a glob.
fn plain_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            let root = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => {
                    Some(PathBuf::from(format!("{}:\\", char::from(drive))))
                }
                Prefix::VerbatimUNC(server, share) => server
                    .to_str()
                    .zip(share.to_str())
                    .map(|(server, share)| PathBuf::from(format!(r"\\{server}\{share}\"))),
                _ => None,
            };
            if let Some(root) = root {
                return components
                    .filter(|component| *component != Component::RootDir)
                    .fold(root, |path, component| path.join(component));
            }
        }
    }
    path.to_path_buf()
}

fn publish(
    original: &Path,
    workspace: &Path,
    epoch_path: &Path,
    epoch_identity: &str,
    epoch_config: &Path,
    home_base: &Path,
) -> Result<PathBuf, String> {
    let (generated, original_config) = config(original, epoch_config)?;
    let credentials = CREDENTIALS
        .iter()
        .map(|name| optional(&original.join(name)))
        .collect::<Result<Vec<_>, _>>()?;
    let epoch_bytes = fs::read(epoch_config)
        .map_err(|error| format!("cannot read dependency epoch configuration: {error}"))?;
    let mut identity = Sha256::new();
    for bytes in [
        b"heli-private-cargo-home-v1".as_slice(),
        original.as_os_str().as_encoded_bytes(),
        workspace.as_os_str().as_encoded_bytes(),
        epoch_identity.as_bytes(),
        generated.as_bytes(),
        &epoch_bytes,
    ] {
        field(&mut identity, bytes);
    }
    for bytes in std::iter::once(&original_config).chain(credentials.iter()) {
        field(&mut identity, &[u8::from(bytes.is_some())]);
        field(&mut identity, bytes.as_deref().unwrap_or_default());
    }
    let name = hex(&identity.finalize());
    private_dir(home_base)?;
    let home = home_base.join(&name);
    let marker = Marker {
        schema: 1,
        original_home: original.to_path_buf(),
        epoch_path: epoch_path.to_path_buf(),
        epoch_identity: epoch_identity.to_owned(),
        config_digest: digest(generated.as_bytes()),
    };
    if home.exists() {
        validate_home(&home, &marker)?;
        return Ok(home);
    }
    let stage = Stage::create(home_base)?;
    private_file(&stage.0.join("config.toml"), generated.as_bytes())?;
    private_file(
        &stage.0.join(MARKER),
        &serde_json::to_vec(&marker).expect("private home metadata serializes"),
    )?;
    for (name, bytes) in CREDENTIALS.iter().zip(credentials) {
        if let Some(bytes) = bytes {
            private_file(&stage.0.join(name), &bytes)?;
        }
    }
    if let Err(error) = fs::rename(&stage.0, &home)
        && !home.exists()
    {
        return Err(format!("cannot publish private Cargo home: {error}"));
    }
    // A concurrent preparer may have won publication. It has the same
    // immutable inputs; validate it without rewriting any active files.
    validate_home(&home, &marker)?;
    Ok(home)
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_home(home: &Path, expected: &Marker) -> Result<(), String> {
    let actual = read_marker(home)?.ok_or("private Cargo home has no publication metadata")?;
    if actual.original_home != expected.original_home
        || actual.epoch_path != expected.epoch_path
        || actual.epoch_identity != expected.epoch_identity
        || actual.config_digest != expected.config_digest
    {
        return Err("private Cargo home publication does not match its selected inputs".to_owned());
    }
    let bytes = optional(&home.join("config.toml"))?
        .ok_or("private Cargo home configuration is missing")?;
    if digest(&bytes) != expected.config_digest {
        return Err("private Cargo home configuration was changed after publication".to_owned());
    }
    check_private(home)?;
    for name in ["config.toml", MARKER, CREDENTIALS[0], CREDENTIALS[1]] {
        let path = home.join(name);
        if path.exists() {
            check_private(&path)?;
        }
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create Cargo home parent directory: {error}"))?;
    }
    match private_new_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "cannot create private Cargo home directory: {error}"
            ));
        }
    }
    check_private(path)
}

fn private_new_dir(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::create_dir(path)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(any(windows, unix)))]
    {
        fs::create_dir(path)
    }
}

fn private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(windows)]
    let opened = windows::create_file(path);
    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    #[cfg(not(any(windows, unix)))]
    let opened = File::options().write(true).create_new(true).open(path);
    opened
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| format!("cannot publish private Cargo home file: {error}"))
}

fn check_private(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect private Cargo home: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("private Cargo home entries cannot be symbolic links".to_owned());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("private Cargo home entries cannot be reparse points".to_owned());
        }
        windows::check(path, metadata.is_dir()).map_err(|error| error.to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(
                "private Cargo home entries must be accessible only to their owner".to_owned(),
            );
        }
    }
    Ok(())
}

/// Only unpublished files created here are removed. A published home is
/// immutable and may belong to another live command; it is never cleaned here.
struct Stage(PathBuf);

impl Stage {
    fn create(base: &Path) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..32 {
            let path = base.join(format!(
                ".stage-{}-{time}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match private_new_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("cannot stage private Cargo home: {error}")),
            }
        }
        Err("cannot allocate a private Cargo home staging directory".to_owned())
    }
}

/// A restrictive descriptor is supplied at creation, not applied after
/// credentials have briefly inherited a custom state's Users-readable ACL.
#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        ERROR_SUCCESS, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
    };

    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Descriptor {
        fn new(sddl: &str) -> io::Result<Self> {
            let sddl = wide(Path::new(sddl));
            let mut descriptor = null_mut();
            // SAFETY: a terminated SDDL string and a descriptor out-pointer.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        fn owner_only() -> io::Result<Self> {
            let sid = crate::ipc::user_sid()?;
            Self::new(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})"))
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by an Advapi32 security-information API.
            unsafe { LocalFree(self.0) };
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    pub(super) fn create_dir(path: &Path) -> io::Result<()> {
        let security = Descriptor::owner_only()?;
        let name = wide(path);
        // SAFETY: the name and non-inheritable security attributes are valid.
        if unsafe { CreateDirectoryW(name.as_ptr(), &security.attributes()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn create_file(path: &Path) -> io::Result<File> {
        let security = Descriptor::owner_only()?;
        let name = wide(path);
        // SAFETY: CREATE_NEW never opens/replaces another run's file. Owner
        // and protected DACL apply before any secret bytes can be written.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                &security.attributes(),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new file handle, owned from here on.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    pub(super) fn check(path: &Path, directory: bool) -> io::Result<()> {
        let name = wide(path);
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: the returned pointers live in descriptor, freed below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor = Descriptor(descriptor);
        let user = Descriptor::owner_only()?;
        let mut expected_owner = null_mut();
        let mut defaulted = 0;
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: both descriptors are valid; all SID/ACL pointers remain live.
        let safe = unsafe {
            GetSecurityDescriptorOwner(user.0, &mut expected_owner, &mut defaulted) != 0
                && !owner.is_null()
                && EqualSid(owner, expected_owner) != 0
                && !dacl.is_null()
                && GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) != 0
                && (!directory || control & SE_DACL_PROTECTED != 0)
        };
        if !safe {
            return Err(denied());
        }
        // Accept only standard allow ACEs for this user, never NULL DACLs,
        // Everyone/Users, unknown conditional ACEs, or group ownership.
        let count = unsafe { (*dacl).AceCount };
        if count == 0 {
            return Err(denied());
        }
        for index in 0..u32::from(count) {
            let mut entry = null_mut();
            // SAFETY: GetAce validates the index and gives an ACE in dacl.
            unsafe {
                if GetAce(dacl, index, &mut entry) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let header = &*entry.cast::<ACE_HEADER>();
                if header.AceType != 0 {
                    return Err(denied());
                }
                let sid = (&raw const (*entry.cast::<ACCESS_ALLOWED_ACE>()).SidStart)
                    .cast_mut()
                    .cast();
                if EqualSid(sid, expected_owner) == 0 {
                    return Err(denied());
                }
                if directory && header.AceFlags & 3 != 3 {
                    return Err(denied());
                }
            }
        }
        Ok(())
    }

    fn denied() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private Cargo home requires its user's owner-only directory/file ACL",
        )
    }

    #[cfg(test)]
    pub(super) fn readable_by_users(path: &Path, create: bool) {
        use windows_sys::Win32::Security::{PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW};
        let sid = crate::ipc::user_sid().unwrap();
        let descriptor =
            Descriptor::new(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;GRGX;;;BU)")).unwrap();
        let name = wide(path);
        // SAFETY: test-owned path only, with deliberately readable security.
        let ok = unsafe {
            if create {
                CreateDirectoryW(name.as_ptr(), &descriptor.attributes())
            } else {
                SetFileSecurityW(
                    name.as_ptr(),
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    descriptor.0,
                )
            }
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let epoch = temp.path().join("epoch");
        fs::create_dir_all(&original).unwrap();
        fs::create_dir_all(&epoch).unwrap();
        fs::write(
            epoch.join("config.toml"),
            "[source.crates-io]\nreplace-with='frozen'\n",
        )
        .unwrap();
        fs::write(
            original.join("config.toml"),
            "[build]\ntarget-dir='relative-target'\n",
        )
        .unwrap();
        fs::write(
            original.join("credentials"),
            "[registry]\ntoken='legacy-secret'\n",
        )
        .unwrap();
        fs::write(
            original.join("credentials.toml"),
            "[registry]\ntoken='current-secret'\n",
        )
        .unwrap();
        let homes = temp.path().join("homes");
        (temp, original, epoch, homes)
    }

    fn select(original: &Path, epoch: &Path, homes: &Path, workspace: &Path) -> PathBuf {
        publish(
            original,
            workspace,
            epoch,
            "epoch-id",
            &epoch.join("config.toml"),
            homes,
        )
        .unwrap()
    }

    #[test]
    fn stable_homes_keep_credentials_private_and_never_overwrite_an_active_home() {
        let (temp, original, epoch, homes) = setup();
        let home = select(&original, &epoch, &homes, temp.path());
        let generated = fs::read_to_string(home.join("config.toml")).unwrap();
        let included: toml::Table = toml::from_str(&generated).unwrap();
        let paths = included["include"].as_array().unwrap();
        assert_eq!(
            Path::new(paths[0].as_str().unwrap()),
            original.join("config.toml")
        );
        assert_eq!(
            Path::new(paths[1].as_str().unwrap()),
            epoch.join("config.toml")
        );
        for name in CREDENTIALS {
            assert_eq!(
                fs::read(home.join(name)).unwrap(),
                fs::read(original.join(name)).unwrap()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(home.join(name)).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        // Model cargo login rotating this private home's credential. A
        // concurrent preparer must not replace it with the original token.
        fs::write(home.join("credentials.toml"), "private rotation").unwrap();
        assert_eq!(select(&original, &epoch, &homes, temp.path()), home);
        assert_eq!(
            fs::read_to_string(home.join("credentials.toml")).unwrap(),
            "private rotation"
        );
        fs::write(original.join("credentials.toml"), "new original credential").unwrap();
        let refreshed = select(&original, &epoch, &homes, temp.path());
        assert_ne!(refreshed, home);
        assert_eq!(
            fs::read_to_string(refreshed.join("credentials.toml")).unwrap(),
            "new original credential"
        );
        assert_eq!(
            fs::read_to_string(home.join("credentials.toml")).unwrap(),
            "private rotation"
        );
    }

    #[test]
    fn independent_worktrees_have_independent_package_cache_homes() {
        let (temp, original, epoch, homes) = setup();
        let first = select(&original, &epoch, &homes, &temp.path().join("first"));
        let second = select(&original, &epoch, &homes, &temp.path().join("second"));
        assert_ne!(first, second);
    }

    #[cfg(windows)]
    #[test]
    fn include_paths_use_plain_absolute_names_instead_of_verbatim_globs() {
        let (temp, original, epoch, homes) = setup();
        let epoch = fs::canonicalize(epoch).unwrap();
        let original = fs::canonicalize(original).unwrap();
        let home = select(&original, &epoch, &homes, temp.path());
        let config: toml::Table =
            toml::from_str(&fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
        for value in config["include"].as_array().unwrap() {
            let text = value.as_str().unwrap();
            assert!(
                !text.contains('?'),
                "Cargo would treat this include as a glob: {text}"
            );
            assert!(Path::new(text).is_absolute());
            assert!(Path::new(text).is_file());
        }
    }

    #[test]
    fn legacy_home_config_is_refused_instead_of_changing_relative_paths() {
        let (temp, original, epoch, homes) = setup();
        fs::write(original.join("config"), "[build]\ntarget-dir='legacy'\n").unwrap();
        let error = publish(
            &original,
            temp.path(),
            &epoch,
            "epoch-id",
            &epoch.join("config.toml"),
            &homes,
        )
        .unwrap_err();
        assert!(error.contains("legacy Cargo home config"), "{error}");
        assert!(!homes.exists());
    }

    #[test]
    fn concurrent_preparers_publish_one_complete_home() {
        let (temp, original, epoch, homes) = setup();
        let selected = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..8 {
                workers.push(scope.spawn(|| select(&original, &epoch, &homes, temp.path())));
            }
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(selected.iter().all(|home| *home == selected[0]));
        assert_eq!(fs::read_dir(&homes).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn readable_state_parent_cannot_expose_credentials_and_unsafe_homes_are_refused() {
        let (temp, original, epoch, _) = setup();
        let shared = temp.path().join("users-readable-state");
        windows::readable_by_users(&shared, true);
        assert!(check_private(&shared).is_err());
        let homes = shared.join("homes");
        let home = select(&original, &epoch, &homes, temp.path());
        // The protected directory boundary and each credential's own ACL
        // are independently validated, despite the weak parent ACL.
        check_private(&homes).unwrap();
        check_private(&home).unwrap();
        for name in CREDENTIALS {
            check_private(&home.join(name)).unwrap();
            assert_eq!(
                fs::read(home.join(name)).unwrap(),
                fs::read(original.join(name)).unwrap()
            );
        }
        windows::readable_by_users(&home, false);
        let result = publish(
            &original,
            temp.path(),
            &epoch,
            "epoch-id",
            &epoch.join("config.toml"),
            &homes,
        );
        assert!(
            result.is_err(),
            "a previously exposed home must not be reused"
        );
        assert_eq!(
            fs::read(home.join("credentials")).unwrap(),
            fs::read(original.join("credentials")).unwrap()
        );
    }
}
