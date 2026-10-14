//! Running a command on a pseudo-terminal, for what heli does differently
//! when its output goes to a terminal: on Windows a pseudo console
//! (ConPTY), elsewhere a pseudo-terminal pair (`posix_openpt`).
//!
//! The command's stdin, stdout and stderr are all the terminal. What it
//! printed comes back as text without terminal escape sequences (colours,
//! cursor moves, window titles) and without carriage returns. A pseudo
//! console redraws its screen, so text that was on one line stays on one
//! line, but runs of spaces may be lost: look for text without them.

use std::process::Command;
use std::time::Duration;

/// How long a command may take on the terminal.
const TIMEOUT: Duration = Duration::from_secs(120);

/// What a command printed on a terminal, and its exit code.
#[derive(Debug)]
pub struct Shown {
    pub code: Option<i32>,
    pub text: String,
}

/// Runs `command` (its program, arguments, environment and working
/// directory) on a new pseudo-terminal; panics after [`TIMEOUT`].
pub fn run(command: &mut Command) -> Shown {
    let (code, bytes) = imp::run(command);
    Shown {
        code,
        text: plain(&bytes),
    }
}

/// `bytes` without escape sequences and carriage returns, lossily decoded.
fn plain(bytes: &[u8]) -> String {
    let mut text = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&byte, after)) = rest.split_first() {
        rest = after;
        match byte {
            b'\r' => {}
            0x1b => match rest.split_first() {
                // CSI: parameters, then a final byte in 0x40..=0x7e.
                Some((b'[', after)) => {
                    let end = after
                        .iter()
                        .position(|byte| (0x40..=0x7e).contains(byte))
                        .map_or(after.len(), |end| end + 1);
                    rest = &after[end..];
                }
                // OSC: up to BEL or ESC \.
                Some((b']', after)) => {
                    let end = after
                        .iter()
                        .enumerate()
                        .find(|&(at, &byte)| {
                            byte == 0x07 || (byte == 0x1b && after.get(at + 1) == Some(&b'\\'))
                        })
                        .map_or(after.len(), |(at, &byte)| {
                            at + if byte == 0x07 { 1 } else { 2 }
                        });
                    rest = &after[end..];
                }
                Some((_, after)) => rest = after,
                None => {}
            },
            _ => text.push(byte),
        }
    }
    String::from_utf8_lossy(&text).into_owned()
}

/// Reads `source` to its end on a thread, and returns what it read once
/// `finished` (the command's exit code) is known and the reading has ended,
/// or a little later: processes the command left behind may keep the
/// terminal open.
fn collect(
    mut source: impl std::io::Read + Send + 'static,
    finished: impl FnOnce() -> Option<i32>,
    after_exit: impl FnOnce(),
) -> (Option<i32>, Vec<u8>) {
    let (chunks, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(read) = source.read(&mut buffer) {
            if read == 0 || chunks.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let code = finished();
    after_exit();
    let mut bytes = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while let Ok(chunk) =
        received.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
    {
        bytes.extend(chunk);
    }
    (code, bytes)
}

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::os::unix::ffi::OsStrExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

    pub fn run(command: &mut Command) -> (Option<i32>, Vec<u8>) {
        let leader =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open a pseudo-terminal");
        grantpt(&leader).expect("grantpt");
        unlockpt(&leader).expect("unlockpt");
        let name = ptsname(&leader, Vec::new()).expect("ptsname");
        let follower = OpenOptions::new()
            .read(true)
            .write(true)
            .open(std::ffi::OsStr::from_bytes(name.as_bytes()))
            .expect("open the terminal");
        command
            .stdin(Stdio::from(follower.try_clone().expect("dup the terminal")))
            .stdout(Stdio::from(follower.try_clone().expect("dup the terminal")))
            .stderr(Stdio::from(follower));
        let mut child = command.spawn().expect("start the command");
        // Drop this process's copies of the terminal, so that reading it
        // ends when the command's do.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        super::collect(
            File::from(leader),
            move || {
                let deadline = Instant::now() + super::TIMEOUT;
                loop {
                    if let Some(status) = child.try_wait().expect("wait for the command") {
                        return status.code();
                    }
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        panic!("the command did not finish on the terminal");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            },
            || {},
        )
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::process::Command;

    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };

    /// A pipe: its read end, its write end.
    fn pipe() -> (HANDLE, HANDLE) {
        let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: both out-pointers are valid.
        let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
        assert!(ok != 0, "CreatePipe: {}", std::io::Error::last_os_error());
        (read, write)
    }

    pub fn run(command: &mut Command) -> (Option<i32>, Vec<u8>) {
        let (input_read, input_write) = pipe();
        let (output_read, output_write) = pipe();
        let mut console: HPCON = 0;
        // SAFETY: the handles are open pipe ends; the console duplicates
        // them, so this process closes its copies below.
        let result = unsafe {
            CreatePseudoConsole(
                COORD { X: 250, Y: 60 },
                input_read,
                output_write,
                0,
                &mut console,
            )
        };
        assert!(result == 0, "CreatePseudoConsole: {result:#x}");
        // SAFETY: handles this process owns and no longer uses.
        unsafe {
            CloseHandle(input_read);
            CloseHandle(output_write);
        }

        let mut size = 0;
        // SAFETY: asks for the size only (it fails, as documented).
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut list = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = list.as_mut_ptr().cast::<c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST;
        // SAFETY: `list` has `size` bytes, aligned for pointers; the console
        // outlives the process creation below.
        unsafe {
            assert!(InitializeProcThreadAttributeList(list, 1, 0, &mut size) != 0);
            assert!(
                UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    console as *const c_void,
                    size_of::<HPCON>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                ) != 0,
                "UpdateProcThreadAttribute: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: all zeros is a valid STARTUPINFOEXW.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = list;
        // Standard handles that are invalid, rather than none: without them
        // the command gets this process's (the test's, often pipes), and its
        // output never reaches the console. With them, it gets the console's.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

        let mut line = wide(&command_line(command));
        let mut environment = environment_block(command);
        let directory = command.get_current_dir().map(|dir| wide(dir.as_os_str()));
        // SAFETY: all zeros is a valid PROCESS_INFORMATION.
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer is valid for the call; no handle is
        // inherited.
        let created = unsafe {
            CreateProcessW(
                std::ptr::null(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_mut_ptr().cast(),
                directory
                    .as_ref()
                    .map_or(std::ptr::null(), |dir| dir.as_ptr()),
                &startup.StartupInfo,
                &mut process,
            )
        };
        assert!(
            created != 0,
            "CreateProcessW: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the list was initialized above; the process has its copy.
        unsafe { DeleteProcThreadAttributeList(list) };
        // SAFETY: handles CreateProcessW returned, owned from here on.
        let (child, thread, output) = unsafe {
            (
                OwnedHandle::from_raw_handle(process.hProcess),
                OwnedHandle::from_raw_handle(process.hThread),
                File::from(OwnedHandle::from_raw_handle(output_read)),
            )
        };
        drop(thread);
        let child_handle = child_raw(&child);
        let (code, bytes) = super::collect(
            output,
            move || {
                let timeout = super::TIMEOUT.as_millis() as u32;
                // SAFETY: a process handle this function owns.
                unsafe {
                    if WaitForSingleObject(child_handle, timeout) != WAIT_OBJECT_0 {
                        TerminateProcess(child_handle, 1);
                        panic!("the command did not finish on the terminal");
                    }
                    let mut code = 0u32;
                    assert!(GetExitCodeProcess(child_handle, &mut code) != 0);
                    Some(code as i32)
                }
            },
            // Closing the console ends its output pipe.
            // SAFETY: the console is not used after this.
            move || unsafe { ClosePseudoConsole(console) },
        );
        drop(child);
        // SAFETY: the write end of the console's input, unused.
        unsafe { CloseHandle(input_write) };
        (code, bytes)
    }

    fn child_raw(child: &OwnedHandle) -> HANDLE {
        std::os::windows::io::AsRawHandle::as_raw_handle(child)
    }

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(Some(0)).collect()
    }

    /// The program and its arguments, quoted as the C runtime and the
    /// standard library parse them.
    fn command_line(command: &Command) -> OsString {
        let mut line = OsString::new();
        for (index, arg) in std::iter::once(command.get_program())
            .chain(command.get_args())
            .enumerate()
        {
            if index > 0 {
                line.push(" ");
            }
            let text = arg.to_string_lossy();
            if !text.is_empty() && !text.contains([' ', '\t', '"']) {
                line.push(arg);
                continue;
            }
            let mut quoted = String::from("\"");
            let mut backslashes = 0;
            for c in text.chars() {
                match c {
                    '\\' => backslashes += 1,
                    '"' => {
                        quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                        backslashes = 0;
                    }
                    _ => {
                        quoted.push_str(&"\\".repeat(backslashes));
                        backslashes = 0;
                    }
                }
                if c != '\\' {
                    quoted.push(c);
                }
            }
            quoted.push_str(&"\\".repeat(backslashes * 2));
            quoted.push('"');
            line.push(quoted);
        }
        line
    }

    /// This process's environment with the command's changes, as the block
    /// `CreateProcessW` takes: `name=value` strings, sorted, each ended by a
    /// NUL, then a NUL.
    fn environment_block(command: &Command) -> Vec<u16> {
        let mut variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        for (name, value) in command.get_envs() {
            variables.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            if let Some(value) = value {
                variables.push((name.to_owned(), value.to_owned()));
            }
        }
        variables.sort_by_key(|(name, _)| name.to_ascii_uppercase());
        let mut block = Vec::new();
        for (name, value) in variables {
            block.extend(name.encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }
}
