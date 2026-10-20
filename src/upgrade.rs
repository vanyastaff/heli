//! `cargo heli self-upgrade` (docs/daemon.md, "Upgrading"): installs a newer
//! cargo-heli beside the running one, swaps it in, and replaces the daemon.
//!
//! `cargo install` cannot replace the program while something runs it: on
//! Windows a mapped image can be neither overwritten nor deleted, and the
//! daemon and every client waiting on a run are such images (an attempt
//! leaves `cargo-heli <version> = []` in the Cargo home's `.crates.toml`,
//! and no program). So the new version is installed into a root of its
//! own, `<state directory>/upgrade`, and moved into place by renaming,
//! which a mapped image allows: the running program goes aside as
//! `<name>.old-<version>` beside it, and the new one takes its name.
//! Elsewhere the new file is renamed over the old one, and the processes
//! running it keep their inode. Then the new program replaces the daemon,
//! as `cargo heli restart` does (`--now` and `--yes` pass through). What an
//! earlier upgrade put aside is deleted first, where nothing runs it any
//! more; `doctor` names what is left ([`aside`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::locations::Locations;

/// Exit code when the upgrade fails before the restart.
const FAILED: i32 = 1;

/// The crate to install.
const CRATE: &str = "cargo-heli";

/// What to install: a version of the crate from crates.io, or a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The crate, at `version` or else the newest.
    Crate { version: Option<String> },
    /// The source directory.
    Path(PathBuf),
}

impl Source {
    /// The arguments that make `cargo install` take this source.
    fn args(&self) -> Vec<OsString> {
        match self {
            Source::Crate { version: None } => vec![OsString::from(CRATE)],
            Source::Crate {
                version: Some(version),
            } => vec![CRATE.into(), "--version".into(), version.into()],
            Source::Path(path) => vec!["--path".into(), path.into()],
        }
    }

    /// What is installed, for messages.
    fn describe(&self) -> String {
        match self {
            Source::Crate { version: None } => format!("the newest {CRATE}"),
            Source::Crate {
                version: Some(version),
            } => format!("{CRATE} {version}"),
            Source::Path(path) => format!("{CRATE} from {}", path.display()),
        }
    }
}

/// `cargo heli self-upgrade`: exits with the restart's code, or 1 when the
/// upgrade fails before it (the program is then unchanged).
pub fn main(now: bool, yes: bool, source: Source) -> ! {
    match upgrade(now, yes, &source) {
        Ok(code) => std::process::exit(code),
        Err(reason) => {
            eprintln!("cargo-heli: self-upgrade: {reason}");
            std::process::exit(FAILED)
        }
    }
}

fn upgrade(now: bool, yes: bool, source: &Source) -> Result<i32, String> {
    let program = std::env::current_exe()
        .map_err(|error| format!("cannot find the running program: {error}"))?;
    let program = crate::store::simplified(&program);
    let locations = Locations::resolve()?;
    for left in remove_aside(&program) {
        eprintln!(
            "cargo-heli: {} is still in use; delete it once nothing runs it",
            left.display()
        );
    }
    let root = locations.state_dir.join("upgrade");
    let cargo = std::env::var_os("CARGO")
        .filter(|cargo| !cargo.is_empty())
        .unwrap_or_else(|| OsString::from("cargo"));
    eprintln!(
        "cargo-heli: installing {} into {}",
        source.describe(),
        root.display()
    );
    let status = Command::new(&cargo)
        .arg("install")
        .args(source.args())
        .arg("--locked")
        .arg("--force")
        .arg("--root")
        .arg(&root)
        // The build must not wait on a saturated pool.
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("MAKEFLAGS")
        .stdin(Stdio::null())
        .status()
        .map_err(|error| format!("cannot run {}: {error}", cargo.to_string_lossy()))?;
    if !status.success() {
        return Err(format!(
            "cargo install exited with {status}; {} is unchanged",
            program.display()
        ));
    }
    let installed = root
        .join("bin")
        .join(format!("{CRATE}{}", std::env::consts::EXE_SUFFIX));
    if !installed.is_file() {
        return Err(format!(
            "cargo install left no {}; {} is unchanged",
            installed.display(),
            program.display()
        ));
    }
    let new_version = version_of(&installed)?;
    let old_version = env!("CARGO_PKG_VERSION");
    swap(&program, &installed, old_version)?;
    eprintln!(
        "cargo-heli: {} is now {new_version} (was {old_version})",
        program.display()
    );
    let mut restart = Command::new(&program);
    restart.arg("restart");
    if now {
        restart.arg("--now");
    }
    if yes {
        restart.arg("--yes");
    }
    let status = restart
        .status()
        .map_err(|error| format!("cannot run {} restart: {error}", program.display()))?;
    Ok(status.code().unwrap_or(FAILED))
}

/// The version `program --version` prints (`cargo-heli 0.0.5`).
fn version_of(program: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .last()
        .filter(|_| output.status.success())
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "{} does not say its version: {}",
                program.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })
}

/// Puts `installed` in `program`'s place: a copy beside it first, so that
/// the last step is a rename on one volume. On Windows the running image
/// goes aside as `<name>.old-<old_version>` (a mapped image can only be
/// renamed); elsewhere the copy is renamed over it.
fn swap(program: &Path, installed: &Path, old_version: &str) -> Result<(), String> {
    let dir = program
        .parent()
        .ok_or_else(|| format!("{} has no directory", program.display()))?;
    let name = program
        .file_name()
        .ok_or_else(|| format!("{} has no name", program.display()))?
        .to_string_lossy();
    let staged = dir.join(format!("{name}.new"));
    std::fs::copy(installed, &staged).map_err(|error| {
        format!(
            "cannot copy {} to {}: {error}",
            installed.display(),
            staged.display()
        )
    })?;
    let _ = std::fs::remove_file(installed);
    if cfg!(windows) {
        let aside = free_name(dir, &format!("{name}.old-{old_version}"));
        if let Err(error) = std::fs::rename(program, &aside) {
            let _ = std::fs::remove_file(&staged);
            return Err(format!(
                "cannot move {} aside to {}: {error}",
                program.display(),
                aside.display()
            ));
        }
        if let Err(error) = std::fs::rename(&staged, program) {
            let restored = std::fs::rename(&aside, program).is_ok();
            let _ = std::fs::remove_file(&staged);
            return Err(format!(
                "cannot put {} in {}'s place: {error}; {}",
                staged.display(),
                program.display(),
                if restored {
                    "the old program is back in place".to_owned()
                } else {
                    format!("the old program is at {}", aside.display())
                }
            ));
        }
    } else if let Err(error) = std::fs::rename(&staged, program) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!(
            "cannot put {} in {}'s place: {error}",
            staged.display(),
            program.display()
        ));
    }
    Ok(())
}

/// `dir/base`, free: deleted when it exists and can be, else `dir/base-2`,
/// `-3`, … the first that does not exist.
fn free_name(dir: &Path, base: &str) -> PathBuf {
    let first = dir.join(base);
    if !first.exists() || std::fs::remove_file(&first).is_ok() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{base}-{n}")))
        .find(|path| !path.exists())
        .expect("a free name")
}

/// The programs an earlier upgrade put aside beside `program`:
/// `<name>.old-*`, sorted.
pub fn aside(program: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (program.parent(), program.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.old-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

/// Deletes what [`aside`] finds, and returns what could not be deleted
/// (still running, on Windows).
fn remove_aside(program: &Path) -> Vec<PathBuf> {
    aside(program)
        .into_iter()
        .filter(|old| std::fs::remove_file(old).is_err())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{Source, aside, free_name, remove_aside, swap};

    #[test]
    fn a_source_gives_cargo_install_its_arguments() {
        let args = |source: &Source| -> Vec<String> {
            source
                .args()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(args(&Source::Crate { version: None }), ["cargo-heli"]);
        assert_eq!(
            args(&Source::Crate {
                version: Some("0.0.9".to_owned())
            }),
            ["cargo-heli", "--version", "0.0.9"]
        );
        assert_eq!(args(&Source::Path(PathBuf::from("src"))), ["--path", "src"]);
        assert_eq!(
            Source::Crate { version: None }.describe(),
            "the newest cargo-heli"
        );
        let _: OsString = OsString::new();
    }

    /// The installed file takes the program's name and place, and the
    /// install root's copy is gone; the old program is beside it on
    /// Windows and gone elsewhere, and an old one left from before is
    /// deleted by the next upgrade.
    #[test]
    fn swap_puts_the_new_program_in_place() {
        let dir = crate::test_tmp::tempdir();
        let bin = dir.path().join("bin");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&root).expect("root");
        let program = bin.join("cargo-heli.exe");
        let installed = root.join("cargo-heli.exe");
        std::fs::write(&program, "old").expect("the program");
        std::fs::write(&installed, "new").expect("the installed program");
        std::fs::write(bin.join("cargo-heli.exe.old-0.0.1"), "older").expect("an old one");
        assert_eq!(remove_aside(&program), Vec::<PathBuf>::new());
        assert_eq!(aside(&program), Vec::<PathBuf>::new());

        swap(&program, &installed, "0.0.2").expect("swap");
        assert_eq!(std::fs::read(&program).expect("the program"), b"new");
        assert!(!installed.exists(), "the install root's copy is gone");
        assert!(
            !bin.join("cargo-heli.exe.new").exists(),
            "nothing is staged"
        );
        let old = bin.join("cargo-heli.exe.old-0.0.2");
        if cfg!(windows) {
            assert_eq!(aside(&program), std::slice::from_ref(&old));
            assert_eq!(std::fs::read(&old).expect("the old program"), b"old");
        } else {
            assert_eq!(aside(&program), Vec::<PathBuf>::new());
        }

        // A name taken by a file that cannot be deleted is not reused.
        std::fs::write(bin.join("taken"), "").expect("a file");
        assert_eq!(free_name(&bin, "free"), bin.join("free"));
        assert_eq!(free_name(&bin, "taken"), bin.join("taken"), "deleted first");
        assert!(!bin.join("taken").exists());
    }
}
