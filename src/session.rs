//! Where this process runs (DESIGN.md §8, `status`): its session, and on
//! Windows its window station and desktop. The daemon must run in the
//! user's interactive session (§3): on Windows that is the window station
//! `WinSta0` of a session other than 0, the services' session.

/// Where a process runs.
#[derive(Debug, Clone, Default)]
pub struct Place {
    /// The logon session on Windows (Terminal Services session id), the
    /// session (`getsid`) elsewhere; `None` when it cannot be read.
    pub session: Option<u32>,
    /// The window station, on Windows only.
    pub window_station: Option<String>,
    /// The desktop of the thread that asked, on Windows only.
    pub desktop: Option<String>,
}

impl Place {
    /// Whether this is the user's interactive desktop: on Windows the
    /// window station `WinSta0` of a session other than 0; `None`
    /// elsewhere, where heli cannot tell.
    pub fn interactive(&self) -> Option<bool> {
        let station = self.window_station.as_deref()?;
        Some(station.eq_ignore_ascii_case("WinSta0") && self.session.is_some_and(|id| id != 0))
    }
}

/// Where this process runs.
#[cfg(windows)]
pub fn here() -> Place {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    let name = |object: HANDLE| {
        if object.is_null() {
            return None;
        }
        let mut buffer = [0u16; 256];
        let mut needed = 0;
        // SAFETY: the buffer and its length in bytes match.
        let ok = unsafe {
            GetUserObjectInformationW(
                object,
                UOI_NAME,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 2) as u32,
                &mut needed,
            )
        };
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        (ok != 0).then(|| String::from_utf16_lossy(&buffer[..end]))
    };
    // SAFETY: plain queries about this process and thread; the handles are
    // not to be closed.
    let (station, desktop) = unsafe {
        (
            GetProcessWindowStation(),
            GetThreadDesktop(GetCurrentThreadId()),
        )
    };
    Place {
        session: Some(crate::locations::session_id()),
        window_station: name(station),
        desktop: name(desktop),
    }
}

/// Where this process runs.
#[cfg(unix)]
pub fn here() -> Place {
    Place {
        session: rustix::process::getsid(None)
            .ok()
            .and_then(|id| u32::try_from(id.as_raw_nonzero().get()).ok()),
        window_station: None,
        desktop: None,
    }
}
