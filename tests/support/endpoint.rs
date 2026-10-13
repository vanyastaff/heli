//! A fixture daemon's endpoint seen from outside: a raw connection, as a
//! client of another heli version would open it, and its access rules.

use std::ffi::OsStr;

/// A raw connection to the daemon at `endpoint`.
#[cfg(unix)]
pub fn connect(endpoint: &OsStr) -> std::os::unix::net::UnixStream {
    std::os::unix::net::UnixStream::connect(endpoint).expect("connect to the daemon")
}

/// A raw connection to the daemon at `endpoint`.
#[cfg(windows)]
pub fn connect(endpoint: &OsStr) -> std::fs::File {
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(endpoint)
        {
            Ok(pipe) => return pipe,
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("connect to the daemon: {error}"),
        }
    }
}

/// One entry of the endpoint's DACL.
#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
pub struct AccessEntry {
    /// An access-allowed entry (else some other type).
    pub allows: bool,
    /// For the user running the tests.
    pub current_user: bool,
}

/// The entries of the DACL of the pipe `endpoint`.
#[cfg(windows)]
pub fn access_entries(endpoint: &OsStr) -> Vec<AccessEntry> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetTokenInformation,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const READ_CONTROL: u32 = 0x0002_0000;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    let pipe = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .open(endpoint)
        .expect("open the pipe to read its security");
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: an open handle; `dacl` points into `descriptor`, freed below.
    let status = unsafe {
        GetSecurityInfo(
            pipe.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, ERROR_SUCCESS, "GetSecurityInfo");
    assert!(!dacl.is_null(), "a NULL DACL lets everyone in");

    // The user running the tests.
    let mut token: HANDLE = null_mut();
    let mut user = vec![0u64; 64];
    let mut needed = 0;
    // SAFETY: queries this process's token into a 512-byte aligned buffer.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        assert_ne!(
            GetTokenInformation(token, TokenUser, user.as_mut_ptr().cast(), 512, &mut needed),
            0
        );
        CloseHandle(token);
    }
    // SAFETY: filled by GetTokenInformation(TokenUser).
    let user_sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut size = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `dacl` is valid while `descriptor` lives.
    let entries = unsafe {
        assert_ne!(
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ),
            0
        );
        (0..size.AceCount)
            .map(|index| {
                let mut ace = null_mut();
                assert_ne!(GetAce(dacl, index, &mut ace), 0);
                let header = &*ace.cast::<ACE_HEADER>();
                let allows = header.AceType == ACCESS_ALLOWED_ACE_TYPE;
                let current_user = allows && {
                    let sid = (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast();
                    EqualSid(sid, user_sid) != 0
                };
                AccessEntry {
                    allows,
                    current_user,
                }
            })
            .collect()
    };
    // SAFETY: allocated by GetSecurityInfo.
    unsafe {
        LocalFree(descriptor);
    }
    entries
}
