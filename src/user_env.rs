//! The Windows user environment (`HKCU\Environment`): the variables every
//! process the user starts gets, terminals, editors and Task Scheduler's
//! logon tasks alike (#82). `setx` writes it; heli reads it to tell a
//! location variable set persistently from one set for one process only
//! (`crate::autostart`), and `cargo heli setup` offers to write
//! `CARGO_MAKEFLAGS` there.
//!
//! **The test seam.** In debug builds, [`VAR`] names a file of `NAME=VALUE`
//! lines that stands in for the registry, read and written instead of it,
//! so that tests never touch the user's real environment. Release builds
//! ignore it. Elsewhere than on Windows there is no user environment: a
//! variable is never found there, and writing is refused.

use std::ffi::{OsStr, OsString};
use std::io;

/// In debug builds, a file that stands in for the registry.
pub const VAR: &str = "HELI_TEST_USER_ENV";

/// The value of `name` in the user environment, if it is set there.
pub fn get(name: &str) -> Option<OsString> {
    if let Some(file) = seam() {
        let text = std::fs::read_to_string(file).ok()?;
        return text
            .lines()
            .filter_map(|line| line.split_once('='))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| OsString::from(value));
    }
    platform::get(name)
}

/// Sets `name` to `value` in the user environment, and tells running
/// programs that it changed.
pub fn set(name: &str, value: &OsStr) -> io::Result<()> {
    if let Some(file) = seam() {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let mut lines: Vec<String> = text
            .lines()
            .filter(|line| {
                !line
                    .split_once('=')
                    .is_some_and(|(key, _)| key.eq_ignore_ascii_case(name))
            })
            .map(str::to_owned)
            .collect();
        lines.push(format!("{name}={}", value.to_string_lossy()));
        return std::fs::write(file, lines.join("\n") + "\n");
    }
    platform::set(name, value)
}

/// The file [`VAR`] names, in a debug build.
fn seam() -> Option<OsString> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var_os(VAR).filter(|file| !file.is_empty())
}

#[cfg(windows)]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegGetValueW, RegSetKeyValueW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(Some(0)).collect()
    }

    pub fn get(name: &str) -> Option<OsString> {
        let key = wide(OsStr::new("Environment"));
        let name = wide(OsStr::new(name));
        let mut size = 0u32;
        // SAFETY: a size query: no buffer, `size` receives the bytes needed.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS || size == 0 {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        // SAFETY: `buffer` holds `size` bytes; REG_EXPAND_SZ values come
        // expanded, as REG_SZ, since RRF_NOEXPAND is not given.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let units = &buffer[..(size as usize / 2)];
        let units = units.strip_suffix(&[0]).unwrap_or(units);
        Some(OsString::from_wide(units))
    }

    pub fn set(name: &str, value: &OsStr) -> io::Result<()> {
        let key = wide(OsStr::new("Environment"));
        let name = wide(OsStr::new(name));
        let data = wide(value);
        // SAFETY: NUL-terminated strings; the data is `data.len()` units.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                u32::try_from(data.len() * 2).map_err(io::Error::other)?,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // As setx does: programs that listen (Explorer, which starts new
        // terminals and editors) read the environment again.
        let area = wide(OsStr::new("Environment"));
        // SAFETY: a broadcast with a NUL-terminated string, bounded in time.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                area.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                std::ptr::null_mut(),
            );
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::io;

    pub fn get(_name: &str) -> Option<OsString> {
        None
    }

    pub fn set(_name: &str, _value: &OsStr) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "there is no user environment to set here",
        ))
    }
}
