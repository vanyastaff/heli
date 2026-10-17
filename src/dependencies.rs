//! Immutable, content-addressed epochs of already-vendored Cargo sources.
//!
//! Readers lock a stable sidecar before looking at a published epoch. Import
//! stages and verifies bytes before taking the catalog lock; GC and publication
//! use that lock, and GC must also exclude every epoch reader. No lease inode
//! is removed, including after retirement and republication of the same epoch.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const ROOT_MARKER: &str = ".heli-dependency-store.json";
const LEASE: &str = ".heli-runtime.lock";
const CHECKSUM: &str = ".cargo-checksum.json";
const PLACEHOLDER: &str = "HELI_DEPENDENCY_EPOCH_VENDOR";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootMarker {
    schema: u32,
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    identity: String,
    packages: BTreeMap<String, String>,
    /// Source-only TOML, with the one directory replacement normalized.
    source_config: String,
}

#[derive(Deserialize)]
// Cargo permits informational envelope fields such as `$comment`. The raw
// checksum file is still hashed, and its checked file inventory stays strict.
struct Checksums {
    files: BTreeMap<String, String>,
    package: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    name: String,
    digest: String,
    executable: bool,
}

/// A validated published epoch, retaining its external lifetime lease.
pub struct Epoch {
    path: PathBuf,
    identity: String,
    config: PathBuf,
    lease_root: PathBuf,
    _lease: File,
}

impl Epoch {
    /// Lock before checking existence: a reader racing retirement must either
    /// see a complete ready epoch or report it unavailable, never use its trash.
    pub fn open(path: &Path) -> Result<Self, String> {
        let identity = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| digest_name(name))
            .ok_or("invalid dependency epoch identity")?;
        let epochs = path.parent().ok_or("epoch has no parent")?;
        if epochs.file_name().is_none_or(|name| name != "epochs") {
            return Err("an epoch must be directly below a store's epochs directory".into());
        }
        let store = epochs.parent().ok_or("epoch has no store")?;
        let store = owned_store(store)?;
        let lease_root = store.join("leases").join(identity);
        let lease = open_lease(&store, identity)?;
        match lease.try_lock_shared() {
            Ok(()) => {},
            Err(TryLockError::WouldBlock) => return Err("dependency epoch is being retired; select a ready epoch or retry after publication".into()),
            Err(TryLockError::Error(problem)) => return Err(error(problem)),
        }
        let path = store.join("epochs").join(identity);
        let manifest = ready(&store, &path)?;
        Ok(Self {
            config: path.join("config.toml"),
            path,
            identity: manifest.identity,
            lease_root,
            _lease: lease,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn config(&self) -> &Path {
        &self.config
    }
    pub fn lease_root(&self) -> &Path {
        &self.lease_root
    }
}

/// Import Cargo vendor output and its emitted source-replacement configuration.
/// File checksums are verified; original archive package digests are preserved
/// for Cargo's own lockfile verification (the archive is not part of this API).
pub fn import(store: &Path, vendor: &Path, config: &Path) -> Result<PathBuf, String> {
    let store = prepare_store(store)?;
    real_dir(vendor)?;
    let vendor = fs::canonicalize(vendor).map_err(error)?;
    regular(config)?;
    let config_text = fs::read_to_string(config).map_err(error)?;
    let sources = normalize_config(&config_text, config, &vendor)?;
    let stage = Stage::create(&store)?;
    let staged_objects = stage.path.join("objects");
    fs::create_dir(&staged_objects).map_err(error)?;
    let mut packages = BTreeMap::new();
    for entry in sorted_entries(&vendor)? {
        real_dir(&entry)?;
        let name = entry
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| package_name(name))
            .ok_or("invalid vendor package directory name")?
            .to_owned();
        let before = inventory(&entry)?;
        let identity = object_identity(&before);
        let destination = staged_objects.join(&identity);
        if !destination.exists() {
            copy_package(&entry, &destination, &before)?;
            let copied = inventory(&destination)?;
            if copied != before {
                return Err(format!("copied package {name} changed during import"));
            }
            readonly_tree(&destination)?;
        }
        if inventory(&entry)? != before {
            return Err(format!("vendor package {name} changed during import"));
        }
        packages.insert(name, identity);
    }
    if packages.is_empty() {
        return Err("vendor directory contains no packages".into());
    }
    // Detect added/removed top-level packages after their individual checks.
    let after_names: BTreeSet<String> = sorted_entries(&vendor)?
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if after_names != packages.keys().cloned().collect() {
        return Err("vendor package set changed during import".into());
    }
    let identity = epoch_identity(&packages, &sources);
    let manifest = Manifest {
        schema: 1,
        identity: identity.clone(),
        packages,
        source_config: sources,
    };
    // This shared lease also excludes a GC retiring an older publication of
    // this identity, and is acquired before the catalog to avoid inversion.
    let lease = open_lease(&store, &identity)?;
    lease.lock_shared().map_err(error)?;
    let _catalog = catalog(&store)?;
    let epoch = store.join("epochs").join(&identity);
    if exists(&epoch) {
        ready_metadata(&store, &epoch)?;
        drop(_catalog);
        ready(&store, &epoch)?;
        return Ok(epoch);
    }
    for object in manifest.packages.values().collect::<BTreeSet<_>>() {
        let target = store.join("objects").join(object);
        if exists(&target) {
            // Never trust an existing object merely because its name matches.
            if object_identity(&inventory(&target)?) != *object {
                return Err(format!("dependency object {object} is corrupt"));
            }
        } else {
            fs::rename(staged_objects.join(object), &target).map_err(error)?;
        }
    }
    let prepared = stage.path.join("epoch");
    fs::create_dir(&prepared).map_err(error)?;
    let view = prepared.join("vendor");
    fs::create_dir(&view).map_err(error)?;
    for (name, object) in &manifest.packages {
        link_directory(&store.join("objects").join(object), &view.join(name))?;
    }
    write_new(
        &prepared.join("config.toml"),
        rendered_config(&manifest, &epoch)?.as_bytes(),
    )?;
    write_new(
        &prepared.join("manifest.json"),
        &serde_json::to_vec(&manifest).map_err(error)?,
    )?;
    fs::rename(&prepared, &epoch).map_err(error)?;
    drop(_catalog);
    ready(&store, &epoch)?;
    Ok(epoch)
}

/// Retire every unleased ready epoch and then unreferenced objects. This is
/// explicit GC, not an automatic capacity policy. Busy epochs retain all refs.
pub fn gc(store: &Path) -> Result<serde_json::Value, String> {
    let store = owned_store(store)?;
    // Hash potentially large source objects under their shared epoch lease,
    // before taking the catalog. A newly published epoch observed only after
    // this pass is retained until the next GC, never hashed under that lock.
    let mut verified = BTreeSet::new();
    for path in sorted_entries(&store.join("epochs"))? {
        let epoch = Epoch::open(&path)?;
        verified.insert(epoch.identity().to_owned());
    }
    let _catalog = catalog(&store)?;
    let mut retired = Vec::new();
    let mut busy = Vec::new();
    let mut retained = BTreeSet::new();
    let mut victims = Vec::new();
    for epoch in sorted_entries(&store.join("epochs"))? {
        let name = epoch
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| digest_name(name))
            .ok_or("unknown entry in dependency epochs")?
            .to_owned();
        let lease = open_lease(&store, &name)?;
        let available = match lease.try_lock() {
            Ok(()) => true,
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(problem)) => return Err(error(problem)),
        };
        let manifest = ready_metadata(&store, &epoch)?;
        if available && verified.contains(&name) {
            victims.push((epoch, name, lease));
        } else {
            retained.extend(manifest.packages.into_values());
            busy.push(name);
        }
    }
    // Validate every prospective object before making the first mutation.
    let objects = sorted_entries(&store.join("objects"))?;
    for object in &objects {
        real_dir(object)?;
        if object
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| !digest_name(name))
        {
            return Err("unknown entry in dependency objects".into());
        }
    }
    let mut retired_leases = Vec::new();
    for (path, name, lease) in victims {
        fs::rename(&path, store.join("trash").join(unique("epoch"))).map_err(error)?;
        retired_leases.push(lease);
        retired.push(name);
    }
    let mut removed_objects = Vec::new();
    for object in objects {
        let name = object.file_name().unwrap().to_string_lossy().into_owned();
        if !retained.contains(&name) {
            fs::rename(&object, store.join("trash").join(unique("object"))).map_err(error)?;
            removed_objects.push(name);
        }
    }
    // Importers hold a stage's lock across heavy work. A crashed stage has
    // no holder; its identity is unique and will never be reused by import.
    for stage in sorted_entries(&store.join("staging"))? {
        real_dir(&stage)?;
        let lock = stage.join(".import.lock");
        if regular(&lock).is_err() {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock)
            .map_err(error)?;
        if file.try_lock().is_ok() {
            drop(file);
            fs::rename(stage, store.join("trash").join(unique("stage"))).map_err(error)?;
        }
    }
    drop(_catalog);
    for trash in sorted_entries(&store.join("trash"))? {
        remove_owned(&trash)?;
    }
    drop(retired_leases);
    Ok(
        serde_json::json!({"schema":1,"retired_epochs":retired,"busy_epochs":busy,"removed_objects":removed_objects}),
    )
}

fn error(problem: impl std::fmt::Display) -> String {
    problem.to_string()
}
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn digest_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn package_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+".contains(&byte))
}
fn linked(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}
fn real_dir(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(error)?;
    if !metadata.is_dir() || linked(&metadata) {
        return Err(format!("not a real directory: {}", path.display()));
    }
    Ok(())
}
fn regular(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path).map_err(error)?;
    if !metadata.is_file() || linked(&metadata) {
        return Err(format!("not a regular file: {}", path.display()));
    }
    Ok(metadata)
}
fn sorted_entries(path: &Path) -> Result<Vec<PathBuf>, String> {
    real_dir(path)?;
    let mut entries = fs::read_dir(path)
        .map_err(error)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(error)?;
    entries.sort();
    Ok(entries)
}

fn prepare_store(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("dependency store must be absolute".into());
    }
    let parent = path.parent().ok_or("store has no parent")?;
    fs::create_dir_all(parent).map_err(error)?;
    match fs::create_dir(path) {
        Ok(()) => {
            let root = fs::canonicalize(path).map_err(error)?;
            write_new(
                &path.join(ROOT_MARKER),
                &serde_json::to_vec(&RootMarker { schema: 1, root }).map_err(error)?,
            )?;
            for name in ["epochs", "objects", "leases", "staging", "trash"] {
                fs::create_dir(path.join(name)).map_err(error)?;
            }
        }
        Err(problem) if problem.kind() == io::ErrorKind::AlreadyExists => {}
        Err(problem) => return Err(error(problem)),
    }
    owned_store(path)
}

fn owned_store(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("dependency store must be absolute".into());
    }
    real_dir(path)?;
    let root = fs::canonicalize(path).map_err(error)?;
    regular(&root.join(ROOT_MARKER))?;
    let marker: RootMarker =
        serde_json::from_slice(&fs::read(root.join(ROOT_MARKER)).map_err(error)?).map_err(error)?;
    if marker.schema != 1 || marker.root != root {
        return Err("unowned dependency store; existing directories are never adopted".into());
    }
    for entry in sorted_entries(&root)? {
        match entry.file_name().and_then(|name| name.to_str()) {
            Some("epochs" | "objects" | "leases" | "staging" | "trash") => real_dir(&entry)?,
            Some(ROOT_MARKER | ".catalog.lock") => {
                regular(&entry)?;
            }
            _ => {
                return Err(format!(
                    "unknown dependency store entry: {}",
                    entry.display()
                ));
            }
        }
    }
    for name in ["epochs", "objects", "leases", "staging", "trash"] {
        real_dir(&root.join(name))?;
    }
    Ok(root)
}

fn catalog(store: &Path) -> Result<File, String> {
    let path = store.join(".catalog.lock");
    if exists(&path) {
        regular(&path)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(error)?;
    file.lock().map_err(error)?;
    Ok(file)
}
fn open_lease(store: &Path, identity: &str) -> Result<File, String> {
    if !digest_name(identity) {
        return Err("invalid epoch lease identity".into());
    }
    let root = store.join("leases").join(identity);
    match fs::create_dir(&root) {
        Ok(()) => {}
        Err(problem) if problem.kind() == io::ErrorKind::AlreadyExists => real_dir(&root)?,
        Err(problem) => return Err(error(problem)),
    }
    let path = root.join(LEASE);
    if exists(&path) {
        regular(&path)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(error)
}

struct Stage {
    path: PathBuf,
    lease: Option<File>,
}
impl Stage {
    fn create(store: &Path) -> Result<Self, String> {
        let _catalog = catalog(store)?;
        let path = store.join("staging").join(unique("import"));
        fs::create_dir(&path).map_err(error)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.join(".import.lock"))
            .map_err(error)?;
        lease.lock().map_err(error)?;
        Ok(Self {
            path,
            lease: Some(lease),
        })
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        // Close before deleting on Windows. This unique staging identity is
        // never reused; a concurrent GC may also remove it, which is harmless.
        drop(self.lease.take());
        let _ = remove_owned(&self.path);
    }
}
fn unique(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{prefix}-{}-{now}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(error)?;
    file.write_all(bytes).map_err(error)?;
    file.sync_all().map_err(error)
}

fn relative_name(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('\\') || name.contains(':') || name.contains('\0') {
        return Err("invalid vendor file name".into());
    }
    let path = Path::new(name);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("vendor path escapes its package".into());
    }
    Ok(path.to_path_buf())
}

fn files(root: &Path) -> Result<Vec<String>, String> {
    fn walk(root: &Path, path: &Path, output: &mut Vec<String>) -> Result<(), String> {
        for child in sorted_entries(path)? {
            let metadata = fs::symlink_metadata(&child).map_err(error)?;
            if linked(&metadata) {
                return Err(format!(
                    "links are not accepted in vendor input: {}",
                    child.display()
                ));
            }
            if metadata.is_dir() {
                walk(root, &child, output)?;
            } else if metadata.is_file() {
                let name = child
                    .strip_prefix(root)
                    .map_err(error)?
                    .components()
                    .map(|part| {
                        part.as_os_str()
                            .to_str()
                            .ok_or("non-Unicode vendor file name")
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join("/");
                relative_name(&name)?;
                output.push(name);
            } else {
                return Err("special files are not accepted in vendor input".into());
            }
        }
        Ok(())
    }
    real_dir(root)?;
    let mut output = Vec::new();
    walk(root, root, &mut output)?;
    output.sort();
    Ok(output)
}

fn inventory(root: &Path) -> Result<Vec<Entry>, String> {
    real_dir(root)?;
    let names = files(root)?;
    let mut checksum_file = input_file(root, Path::new(CHECKSUM))?;
    let mut bytes = Vec::new();
    checksum_file.read_to_end(&mut bytes).map_err(error)?;
    let checksums: Checksums = serde_json::from_slice(&bytes).map_err(error)?;
    if checksums
        .package
        .as_ref()
        .is_some_and(|digest| !digest_name(digest))
    {
        return Err("invalid original crate archive checksum".into());
    }
    let expected: BTreeSet<_> = checksums.files.keys().cloned().collect();
    let actual: BTreeSet<_> = names
        .iter()
        .filter(|name| name.as_str() != CHECKSUM)
        .cloned()
        .collect();
    if expected != actual {
        return Err(format!(
            "vendor checksum inventory differs from actual files: {}",
            root.display()
        ));
    }
    let mut output = Vec::new();
    for name in names {
        let mut file = input_file(root, &relative_name(&name)?)?;
        let executable = executable(&file.metadata().map_err(error)?);
        let digest = hash_reader(&mut file)?;
        if name != CHECKSUM && checksums.files.get(&name) != Some(&digest) {
            return Err(format!(
                "vendor checksum mismatch: {}",
                root.join(&name).display()
            ));
        }
        output.push(Entry {
            name,
            digest,
            executable,
        });
    }
    // Cargo's checksum map lists files, but empty directories can still be
    // observable by build scripts. Retain and identify directory names too.
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for path in sorted_entries(&directory)? {
            let metadata = fs::symlink_metadata(&path).map_err(error)?;
            if linked(&metadata) {
                return Err("link appeared during vendor inventory".into());
            }
            if metadata.is_dir() {
                let name = path
                    .strip_prefix(root)
                    .map_err(error)?
                    .components()
                    .map(|part| {
                        part.as_os_str()
                            .to_str()
                            .ok_or("non-Unicode vendor directory")
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join("/");
                output.push(Entry {
                    name: format!("{name}/"),
                    digest: "directory".into(),
                    executable: false,
                });
                pending.push(path);
            }
        }
    }
    output.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(output)
}
fn executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}
fn hash_reader(reader: &mut impl Read) -> Result<String, String> {
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut bytes).map_err(error)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
fn object_identity(entries: &[Entry]) -> String {
    let mut hash = Sha256::new();
    field(&mut hash, b"heli-vendor-object-v1");
    for entry in entries {
        field(&mut hash, entry.name.as_bytes());
        field(&mut hash, entry.digest.as_bytes());
        field(&mut hash, &[u8::from(entry.executable)]);
    }
    format!("{:x}", hash.finalize())
}
fn epoch_identity(packages: &BTreeMap<String, String>, sources: &str) -> String {
    let mut hash = Sha256::new();
    field(&mut hash, b"heli-source-epoch-v1");
    field(&mut hash, sources.as_bytes());
    for (name, object) in packages {
        field(&mut hash, name.as_bytes());
        field(&mut hash, object.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn copy_package(source: &Path, target: &Path, entries: &[Entry]) -> Result<(), String> {
    fs::create_dir(target).map_err(error)?;
    for entry in entries {
        let relative = relative_name(&entry.name)?;
        if entry.name.ends_with('/') {
            fs::create_dir_all(target.join(relative)).map_err(error)?;
            continue;
        }
        let mut source = input_file(source, &relative)?;
        let destination = target.join(relative);
        fs::create_dir_all(destination.parent().ok_or("file has no parent")?).map_err(error)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(error)?;
        io::copy(&mut source, &mut file).map_err(error)?;
        file.sync_all().map_err(error)?;
        fs::set_permissions(
            &destination,
            source.metadata().map_err(error)?.permissions(),
        )
        .map_err(error)?;
    }
    Ok(())
}

/// Resolve every component relative to a real source directory. A no-follow
/// final open alone would still permit an intermediate directory escape.
#[cfg(unix)]
fn input_file(root: &Path, relative: &Path) -> Result<File, String> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let mut directory = File::from(
        open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(error)?,
    );
    let parts: Vec<_> = relative.components().collect();
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err("input path escapes package".into());
        };
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let flags = if index + 1 == parts.len() {
            flags
        } else {
            flags | OFlags::DIRECTORY
        };
        directory = File::from(openat(&directory, *name, flags, Mode::empty()).map_err(error)?);
    }
    if !directory.metadata().map_err(error)?.is_file() {
        return Err("vendor input is not a regular file".into());
    }
    Ok(directory)
}

#[cfg(windows)]
fn input_file(root: &Path, relative: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, GetFinalPathNameByHandleW,
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(root.join(relative))
        .map_err(error)?;
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file() || linked(&metadata) {
        return Err("vendor input is not a regular file".into());
    }
    let mut path = vec![0u16; 32768];
    // SAFETY: the live file owns the handle, and the output buffer is writable.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            path.as_mut_ptr(),
            path.len() as u32,
            0,
        )
    };
    if length == 0 || length as usize >= path.len() {
        return Err("cannot resolve vendor input handle".into());
    }
    let actual = PathBuf::from(String::from_utf16(&path[..length as usize]).map_err(error)?);
    let expected = fs::canonicalize(root).map_err(error)?.join(relative);
    if !crate::store::same_path(&actual, &expected) {
        return Err("vendor input escaped its package through a directory link".into());
    }
    Ok(file)
}

fn normalize_config(text: &str, config: &Path, vendor: &Path) -> Result<String, String> {
    let mut table = source_table(text)?;
    if table.len() != 1 || !table.contains_key("source") {
        return Err("vendor configuration must contain only source replacement tables".into());
    }
    let sources = table
        .get_mut("source")
        .and_then(toml::Value::as_table_mut)
        .ok_or("source must be a table")?;
    let mut directory_name = None;
    for (name, source) in sources.iter_mut() {
        let source = source
            .as_table_mut()
            .ok_or("source replacement entry is not a table")?;
        for (key, value) in source.iter() {
            if !matches!(
                key.as_str(),
                "replace-with" | "directory" | "registry" | "git" | "rev" | "tag" | "branch"
            ) || !value.is_str()
            {
                return Err("unsupported vendor source configuration".into());
            }
            if matches!(key.as_str(), "registry" | "git") {
                let value = value.as_str().unwrap();
                if value.contains(['\r', '\n', '\0'])
                    || value.split_once("://").is_some_and(|(_, rest)| {
                        rest.split('/')
                            .next()
                            .is_some_and(|authority| authority.contains('@'))
                    })
                {
                    return Err(
                        "credentials or control characters in vendor source identity".into(),
                    );
                }
            }
        }
        let pure_directory = source.len() == 1;
        if let Some(directory) = source.get_mut("directory") {
            if directory_name.is_some() || !pure_directory {
                return Err("expected one pure directory replacement source".into());
            }
            let directory_text = directory
                .as_str()
                .ok_or("directory replacement is not a string")?;
            let path = Path::new(directory_text);
            let from_config = config.parent().unwrap_or(Path::new(".")).join(path);
            let from_cwd = std::env::current_dir().map_err(error)?.join(path);
            if fs::canonicalize(from_config).ok().as_deref() != Some(vendor)
                && fs::canonicalize(from_cwd).ok().as_deref() != Some(vendor)
            {
                return Err("source directory does not name the supplied vendor directory".into());
            }
            *directory = toml::Value::String(PLACEHOLDER.into());
            directory_name = Some(name.clone());
        }
    }
    let directory = directory_name.ok_or("vendor configuration has no directory source")?;
    for (name, _) in sources.iter() {
        if name == &directory {
            continue;
        }
        let mut visited = BTreeSet::new();
        let mut current = name.as_str();
        while current != directory {
            if !visited.insert(current.to_owned()) {
                return Err("cyclic source replacement".into());
            }
            current = sources
                .get(current)
                .and_then(toml::Value::as_table)
                .and_then(|source| source.get("replace-with"))
                .and_then(toml::Value::as_str)
                .ok_or("source replacement does not reach the vendor directory")?;
        }
    }
    let normalized = toml::to_string(&table).map_err(error)?;
    validate_template(&normalized)?;
    Ok(normalized)
}

fn validate_template(text: &str) -> Result<(), String> {
    let table = source_table(text)?;
    if table.len() != 1 {
        return Err("epoch configuration must be source-only".into());
    }
    let sources = table
        .get("source")
        .and_then(toml::Value::as_table)
        .ok_or("epoch has no source tables")?;
    let mut directory_name = None;
    for (name, source) in sources {
        if name.contains(['\r', '\n', '\0'])
            || name.split_once("://").is_some_and(|(_, rest)| {
                rest.split('/')
                    .next()
                    .is_some_and(|authority| authority.contains('@'))
            })
        {
            return Err("credentials or control characters in source name".into());
        }
        let source = source.as_table().ok_or("source entry is not a table")?;
        if source.is_empty() {
            return Err("empty source entry".into());
        }
        for (key, value) in source {
            if !matches!(
                key.as_str(),
                "replace-with" | "directory" | "registry" | "git" | "rev" | "tag" | "branch"
            ) || !value.is_str()
            {
                return Err("unknown epoch source configuration".into());
            }
            let value = value.as_str().unwrap();
            if value.contains(['\r', '\n', '\0'])
                || (matches!(key.as_str(), "registry" | "git")
                    && (value.contains(['?', '#'])
                        || value.split_once("://").is_some_and(|(_, rest)| {
                            rest.split('/')
                                .next()
                                .is_some_and(|authority| authority.contains('@'))
                        })))
            {
                return Err("credentials or unsupported query in source identity".into());
            }
        }
        if let Some(directory) = source.get("directory") {
            if directory.as_str() != Some(PLACEHOLDER)
                || source.len() != 1
                || directory_name.is_some()
            {
                return Err("invalid normalized directory source".into());
            }
            directory_name = Some(name.as_str());
        }
    }
    let directory = directory_name.ok_or("epoch has no directory replacement")?;
    for name in sources.keys() {
        let mut current = name.as_str();
        let mut visited = BTreeSet::new();
        while current != directory {
            if !visited.insert(current) {
                return Err("cyclic epoch source replacement".into());
            }
            current = sources
                .get(current)
                .and_then(toml::Value::as_table)
                .and_then(|source| source.get("replace-with"))
                .and_then(toml::Value::as_str)
                .ok_or("epoch source does not reach directory replacement")?;
        }
    }
    Ok(())
}

fn rendered_config(manifest: &Manifest, path: &Path) -> Result<String, String> {
    validate_template(&manifest.source_config)?;
    let mut table = source_table(&manifest.source_config)?;
    let sources = table
        .get_mut("source")
        .and_then(toml::Value::as_table_mut)
        .ok_or("epoch has no source tables")?;
    let mut rewritten = 0;
    for (_, source) in sources.iter_mut() {
        if let Some(directory) = source
            .as_table_mut()
            .and_then(|table| table.get_mut("directory"))
        {
            if directory.as_str() != Some(PLACEHOLDER) {
                return Err("invalid epoch directory replacement".into());
            }
            *directory = toml::Value::String(
                path.join("vendor")
                    .to_str()
                    .ok_or("non-Unicode epoch path")?
                    .into(),
            );
            rewritten += 1;
        }
    }
    if rewritten != 1 {
        return Err("epoch must have one directory source".into());
    }
    toml::to_string(&table).map_err(error)
}

/// TOML diagnostics include the offending input line. The wrong input may be
/// a credential-bearing Cargo configuration, so never echo parser details.
fn source_table(text: &str) -> Result<toml::Table, String> {
    toml::from_str(text).map_err(|_| {
        "invalid source replacement TOML; provide Cargo vendor's source-only output".into()
    })
}

fn ready(store: &Path, path: &Path) -> Result<Manifest, String> {
    let manifest = ready_metadata(store, path)?;
    for object in manifest.packages.values().collect::<BTreeSet<_>>() {
        let expected = store.join("objects").join(object);
        if object_identity(&inventory(&expected)?) != *object {
            return Err(format!(
                "dependency object {object} content differs from its identity"
            ));
        }
    }
    Ok(manifest)
}

fn ready_metadata(store: &Path, path: &Path) -> Result<Manifest, String> {
    real_dir(path)?;
    regular(&path.join("manifest.json"))?;
    regular(&path.join("config.toml"))?;
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(path.join("manifest.json")).map_err(error)?)
            .map_err(error)?;
    if manifest.schema != 1
        || !digest_name(&manifest.identity)
        || path.file_name().and_then(|name| name.to_str()) != Some(manifest.identity.as_str())
        || epoch_identity(&manifest.packages, &manifest.source_config) != manifest.identity
    {
        return Err("invalid dependency epoch manifest".into());
    }
    if fs::read_to_string(path.join("config.toml")).map_err(error)?
        != rendered_config(&manifest, path)?
    {
        return Err("epoch source configuration differs from its manifest".into());
    }
    let view = path.join("vendor");
    real_dir(&view)?;
    let root_entries: BTreeSet<_> = sorted_entries(path)?
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if root_entries
        != ["vendor", "config.toml", "manifest.json"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    {
        return Err("unknown entry in dependency epoch".into());
    }
    let actual: BTreeSet<_> = sorted_entries(&view)?
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    if actual != manifest.packages.keys().cloned().collect() {
        return Err("epoch package view differs from manifest".into());
    }
    for (name, object) in &manifest.packages {
        if !package_name(name) || !digest_name(object) {
            return Err("invalid epoch package identity".into());
        }
        let expected = store.join("objects").join(object);
        real_dir(&expected)?;
        let link = view.join(name);
        if !linked(&fs::symlink_metadata(&link).map_err(error)?)
            || fs::canonicalize(&link).map_err(error)? != expected
        {
            return Err("epoch package view does not reference its immutable object".into());
        }
    }
    Ok(manifest)
}

#[cfg(windows)]
fn link_directory(target: &Path, path: &Path) -> Result<(), String> {
    junction::create(target, path).map_err(error)
}
#[cfg(unix)]
fn link_directory(target: &Path, path: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, path).map_err(error)
}

fn readonly_tree(root: &Path) -> Result<(), String> {
    for name in files(root)? {
        let path = root.join(relative_name(&name)?);
        let mut permissions = regular(&path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).map_err(error)?;
    }
    Ok(())
}

/// Only used below an owned store. Inspect each child and unlink reparse
/// points themselves: recursive deletion must never traverse epoch views.
fn remove_owned(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(problem) if problem.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(problem) => return Err(error(problem)),
    };
    if linked(&metadata) {
        #[cfg(windows)]
        {
            return removed(fs::remove_dir(path).or_else(|_| fs::remove_file(path)));
        }
        #[cfg(unix)]
        {
            return removed(fs::remove_file(path));
        }
    }
    if metadata.is_dir() {
        let children = match sorted_entries(path) {
            Ok(children) => children,
            Err(_) if !exists(path) => return Ok(()),
            Err(problem) => return Err(problem),
        };
        for child in children {
            remove_owned(&child)?;
        }
        removed(fs::remove_dir(path))
    } else if metadata.is_file() {
        #[cfg(windows)]
        #[allow(
            clippy::permissions_set_readonly_false,
            reason = "Windows read-only attribute must be cleared before deleting an owned object; Unix permissions are unchanged"
        )]
        {
            if metadata.permissions().readonly() {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                removed(fs::set_permissions(path, permissions))?;
            }
        }
        removed(fs::remove_file(path))
    } else {
        Err("refusing to delete a special dependency-store file".into())
    }
}

fn removed(result: io::Result<()>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(problem) if problem.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(problem) => Err(error(problem)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vendor(root: &Path, content: &str) -> (PathBuf, PathBuf) {
        let vendor = root.join("vendor");
        let package = vendor.join("tiny-1.0.0");
        fs::create_dir_all(package.join("src")).unwrap();
        let files = [
            ("Cargo.toml", "[package]\nname='tiny'\nversion='1.0.0'\n"),
            ("src/lib.rs", content),
        ];
        let mut sums = serde_json::Map::new();
        for (name, text) in files {
            fs::write(package.join(name), text).unwrap();
            sums.insert(
                name.into(),
                format!("{:x}", Sha256::digest(text.as_bytes())).into(),
            );
        }
        fs::write(
            package.join(CHECKSUM),
            serde_json::json!({"$comment":"Cargo vendor includes informational metadata","files":sums,"package":"a".repeat(64)}).to_string(),
        )
        .unwrap();
        let config = root.join("sources.toml");
        fs::write(
            &config,
            "[source.crates-io]\nreplace-with='vendor'\n[source.vendor]\ndirectory='vendor'\n",
        )
        .unwrap();
        (vendor, config)
    }

    #[test]
    fn importing_twice_deduplicates_and_a_reader_excludes_gc() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let (vendor, config) = vendor(temp.path(), "pub fn answer() -> u8 { 42 }\n");
        let first = import(&store, &vendor, &config).unwrap();
        assert_eq!(first, import(&store, &vendor, &config).unwrap());
        assert_eq!(sorted_entries(&store.join("objects")).unwrap().len(), 1);
        let epoch = Epoch::open(&first).unwrap();
        assert_eq!(
            fs::read_to_string(epoch.path().join("vendor/tiny-1.0.0/src/lib.rs")).unwrap(),
            "pub fn answer() -> u8 { 42 }\n"
        );
        assert_eq!(
            gc(&store).unwrap()["busy_epochs"].as_array().unwrap().len(),
            1
        );
        assert!(first.exists());
        let lease = epoch.lease_root().join(LEASE);
        drop(epoch);
        // A child another test spawns holds a copy of the lease's descriptor
        // (and with it the flock) until it execs: until then the epoch is
        // busy, as GC rightly says. Only a lease that stays held fails.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let report = loop {
            let report = gc(&store).unwrap();
            if report["busy_epochs"].as_array().unwrap().is_empty()
                || std::time::Instant::now() >= deadline
            {
                break report;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(report["retired_epochs"].as_array().unwrap().len(), 1);
        assert!(!first.exists());
        assert!(lease.is_file());
        assert!(sorted_entries(&store.join("objects")).unwrap().is_empty());
    }

    #[test]
    fn an_existing_empty_directory_is_not_adopted() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        fs::create_dir(&store).unwrap();
        let (vendor, config) = vendor(temp.path(), "pub const ANSWER: u8 = 42;\n");
        assert!(import(&store, &vendor, &config).is_err());
        assert!(sorted_entries(&store).unwrap().is_empty());
    }

    #[test]
    fn corrupted_files_and_checksum_path_escapes_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let (vendor, config) = vendor(temp.path(), "pub const ANSWER: u8 = 42;\n");
        fs::write(vendor.join("tiny-1.0.0/src/lib.rs"), "different bytes").unwrap();
        assert!(
            import(&store, &vendor, &config)
                .unwrap_err()
                .contains("checksum mismatch")
        );
        fs::write(
            vendor.join("tiny-1.0.0").join(CHECKSUM),
            serde_json::json!({"files":{"../secret":"a".repeat(64)},"package":null}).to_string(),
        )
        .unwrap();
        assert!(import(&store, &vendor, &config).is_err());
        assert!(sorted_entries(&store.join("epochs")).unwrap().is_empty());
    }

    #[test]
    fn credentials_and_non_source_configuration_are_not_imported() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let (vendor, config) = vendor(temp.path(), "pub const ANSWER: u8 = 42;\n");
        fs::write(&config, "[registry]\ntoken='secret'\n").unwrap();
        assert!(import(&store, &vendor, &config).is_err());
        assert!(sorted_entries(&store.join("epochs")).unwrap().is_empty());
    }

    #[test]
    fn malformed_source_configuration_does_not_echo_credentials() {
        const SECRET: &str = "HELI_DEPENDENCY_CONFIG_SECRET_SENTINEL";
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let (vendor, config) = vendor(temp.path(), "pub const ANSWER: u8 = 42;\n");
        let malformed = format!("[registry]\ntoken = \"{SECRET}\n");
        // The raw TOML parser includes this unfinished string's input line.
        assert!(
            toml::from_str::<toml::Table>(&malformed)
                .unwrap_err()
                .to_string()
                .contains(SECRET)
        );
        fs::write(&config, &malformed).unwrap();
        let problem = import(&store, &vendor, &config).unwrap_err();
        assert!(
            !problem.contains(SECRET),
            "credential echoed by import: {problem}"
        );
        assert!(sorted_entries(&store.join("epochs")).unwrap().is_empty());
        let manifest = Manifest {
            schema: 1,
            identity: "a".repeat(64),
            packages: BTreeMap::new(),
            source_config: malformed,
        };
        let problem =
            rendered_config(&manifest, &store.join("epochs").join(&manifest.identity)).unwrap_err();
        assert!(
            !problem.contains(SECRET),
            "credential echoed by epoch validation: {problem}"
        );
    }
}
