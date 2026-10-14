//! A run's process tree: the run's process and every process it starts,
//! however deep, so that `cargo heli cancel` can kill all of them.
//!
//! - **Windows**: a Job Object per run. The run's process starts
//!   suspended, joins the job, and only then runs, so nothing it starts
//!   escapes: the job allows no breakaway. Jobs nest, so the jobs cargo and
//!   nextest make for themselves become the run's job's children. The job
//!   does not kill its processes when its last handle closes, so runs
//!   outlive a daemon that dies.
//! - **Unix**: a session of its own (`setsid`). A process group would not
//!   do: nextest puts each test in a process group of its own. A process
//!   leaves the session only by starting a session of its own, as daemons
//!   do.
//!
//! [`Tree::kill`]: on Windows, `TerminateJobObject`; on Unix, SIGTERM to
//! every process of the session, so that cargo and nextest can stop their
//! children, then, after up to [`TERM_GRACE`], SIGKILL to those left.
//! Either way it returns once no process of the tree is left, or fails
//! after [`KILL_WAIT`].

use std::io;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// How long processes may take to exit after SIGTERM before they get
/// SIGKILL.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_secs(2);

/// How long the processes of a tree being killed may take to be gone.
const KILL_WAIT: Duration = Duration::from_secs(10);

/// How often a tree being killed is looked at.
const POLL: Duration = Duration::from_millis(20);

/// The process tree of one run.
pub struct Tree {
    /// The run's job, or why it has none.
    #[cfg(windows)]
    job: Result<windows::Job, String>,
    /// The run's session: its process's id.
    #[cfg(unix)]
    session: rustix::process::Pid,
}

/// Starts `command` as the first process of a new tree.
pub fn spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
    platform_spawn(command)
}

impl Tree {
    /// Kills every process of the tree; returns once none is left.
    pub fn kill(&self) -> io::Result<()> {
        self.platform_kill()
    }
}

/// Waits until `gone` holds, [`KILL_WAIT`] at most; `what` is left
/// otherwise.
fn wait_until_gone(what: &str, gone: impl Fn() -> io::Result<bool>) -> io::Result<()> {
    let deadline = Instant::now() + KILL_WAIT;
    while !gone()? {
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "{what} still running after {} s",
                KILL_WAIT.as_secs()
            )));
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

#[cfg(windows)]
fn platform_spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    let mut child = command.creation_flags(CREATE_SUSPENDED).spawn()?;
    // A run without a job still runs: only cancel cannot reach all of it.
    let job = windows::Job::new()
        .and_then(|job| job.assign(&child).map(|()| job))
        .map_err(|error| format!("its processes are in no job: {error}"));
    if let Err(error) = windows::resume(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, Tree { job }))
}

#[cfg(windows)]
impl Tree {
    fn platform_kill(&self) -> io::Result<()> {
        let job = self
            .job
            .as_ref()
            .map_err(|why| io::Error::other(why.clone()))?;
        // The job stops counting a process a moment before the process has
        // ended, so its processes are waited for too.
        let processes = job.processes()?;
        job.terminate()?;
        wait_until_gone("some of its processes are", || {
            Ok(job.active_processes()? == 0 && processes.iter().all(windows::ended))
        })
    }
}

#[cfg(unix)]
fn platform_spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setsid` is a system call, safe between fork and exec.
    unsafe {
        command.pre_exec(|| rustix::process::setsid().map(drop).map_err(io::Error::from));
    }
    let child = command.spawn()?;
    let session = rustix::process::Pid::from_child(&child);
    Ok((child, Tree { session }))
}

#[cfg(unix)]
impl Tree {
    fn platform_kill(&self) -> io::Result<()> {
        use rustix::process::Signal;

        unix::signal(self.session, Signal::TERM);
        let deadline = Instant::now() + TERM_GRACE;
        while !unix::members(self.session).is_empty() && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        wait_until_gone("some of its processes are", || {
            let gone = unix::members(self.session).is_empty();
            if !gone {
                unix::signal(self.session, Signal::KILL);
            }
            Ok(gone)
        })
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, QueryInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenThread, PROCESS_SYNCHRONIZE, ResumeThread, THREAD_SUSPEND_RESUME,
        WaitForSingleObject,
    };

    /// The exit code of the processes of a cancelled run, as for a process
    /// the standard library kills.
    const CANCELLED: u32 = 1;

    /// A Job Object, with no limits.
    pub struct Job(OwnedHandle);

    impl Job {
        pub fn new() -> io::Result<Self> {
            // SAFETY: an unnamed job with default security.
            let handle = unsafe { CreateJobObjectW(null(), null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a fresh handle, owned from here on.
            Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
        }

        /// Puts `child`, which has not run yet, in the job.
        pub fn assign(&self, child: &Child) -> io::Result<()> {
            // SAFETY: both handles are open; the standard library's process
            // handle has every access right.
            let ok =
                unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Handles to the processes in the job (and in the jobs nested in
        /// it) now, to wait for.
        pub fn processes(&self) -> io::Result<Vec<OwnedHandle>> {
            let mut capacity = 64;
            loop {
                // The list's two counts, then `capacity` process ids.
                let bytes = 8 + capacity * size_of::<usize>();
                let mut buffer = vec![0u64; bytes.div_ceil(8)];
                // SAFETY: the buffer holds `bytes` bytes, aligned for the
                // list.
                let ok = unsafe {
                    QueryInformationJobObject(
                        self.0.as_raw_handle(),
                        JobObjectBasicProcessIdList,
                        buffer.as_mut_ptr().cast(),
                        u32::try_from(bytes).unwrap_or(u32::MAX),
                        null_mut(),
                    )
                };
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_MORE_DATA as i32) {
                        capacity *= 4;
                        continue;
                    }
                    return Err(error);
                }
                let list = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
                // SAFETY: the call filled the list, with this many ids.
                let ids = unsafe {
                    std::slice::from_raw_parts(
                        (&raw const (*list).ProcessIdList).cast::<usize>(),
                        (*list).NumberOfProcessIdsInList as usize,
                    )
                };
                return Ok(ids
                    .iter()
                    .filter_map(|&id| {
                        // SAFETY: opens a handle, owned below; a process
                        // that has ended meanwhile is not opened.
                        let handle =
                            unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, u32::try_from(id).ok()?) };
                        // SAFETY: a fresh handle.
                        (!handle.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
                    })
                    .collect());
            }
        }

        /// Ends every process in the job and in the jobs nested in it.
        pub fn terminate(&self) -> io::Result<()> {
            // SAFETY: a job handle with every access right.
            if unsafe { TerminateJobObject(self.0.as_raw_handle(), CANCELLED) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// How many processes in the job are running.
        pub fn active_processes(&self) -> io::Result<u32> {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: `info` is the structure this class fills, with its size.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(info.ActiveProcesses)
        }
    }

    /// Whether `process` (a handle with SYNCHRONIZE access) has ended.
    pub fn ended(process: &OwnedHandle) -> bool {
        // SAFETY: an open process handle; does not wait.
        unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    }

    /// Resumes the threads of `child`, started suspended: its one thread.
    /// The standard library keeps no handle to it, so it is found in a
    /// snapshot of the system's threads.
    pub fn resume(child: &Child) -> io::Result<()> {
        // SAFETY: a snapshot of every thread; closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle, owned from here on.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut resumed = 0;
        // SAFETY: `entry` has its size set, as the snapshot calls require.
        let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == child.id() {
                // SAFETY: opens the thread by id; closed below.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if !thread.is_null() {
                    // SAFETY: a thread handle just opened, owned from here on.
                    let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                    // SAFETY: a thread handle with THREAD_SUSPEND_RESUME.
                    if unsafe { ResumeThread(thread.as_raw_handle()) } != u32::MAX {
                        resumed += 1;
                    }
                }
            }
            // SAFETY: as above.
            more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
        }
        if resumed == 0 {
            return Err(io::Error::other("cannot resume the process"));
        }
        Ok(())
    }
}

#[cfg(unix)]
mod unix {
    use rustix::process::{Pid, Signal, kill_process, kill_process_group};

    /// Sends `signal` to every process of `session`.
    pub fn signal(session: Pid, signal: Signal) {
        // The group of the session's leader first, at once.
        let _ = kill_process_group(session, signal);
        for pid in members(session) {
            let _ = kill_process(pid, signal);
        }
    }

    /// The live processes of `session` (not those dead and waiting to be
    /// reaped).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn members(session: Pid) -> Vec<Pid> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let session = session.as_raw_nonzero().get();
        entries
            .filter_map(|entry| {
                let pid: i32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
                // `pid (comm) state ppid pgrp session …`; the name may hold
                // anything, so the fields are counted after its last `)`.
                let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
                let name_end = stat.iter().rposition(|&byte| byte == b')')?;
                let fields = std::str::from_utf8(&stat[name_end + 1..]).ok()?;
                let mut fields = fields.split_ascii_whitespace();
                let state = fields.next()?;
                let sid: i32 = fields.nth(2)?.parse().ok()?;
                if sid != session || matches!(state, "Z" | "X" | "x") {
                    return None;
                }
                Pid::from_raw(pid)
            })
            .collect()
    }

    /// The live processes of `session` (not those dead and waiting to be
    /// reaped).
    #[cfg(target_os = "macos")]
    pub fn members(session: Pid) -> Vec<Pid> {
        let session = session.as_raw_nonzero().get();
        let Ok(pids) = crate::processes::pids() else {
            return Vec::new();
        };
        pids.into_iter()
            // SAFETY: `getsid` only queries.
            .filter(|&pid| pid > 0 && unsafe { libc::getsid(pid) } == session && !zombie(pid))
            .filter_map(Pid::from_raw)
            .collect()
    }

    /// Whether process `pid` is dead and waiting to be reaped, or gone.
    #[cfg(target_os = "macos")]
    fn zombie(pid: libc::pid_t) -> bool {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as std::ffi::c_int;
        // SAFETY: the buffer holds `size` bytes.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        // SAFETY: zeroed, then filled by the call.
        filled != size || unsafe { info.assume_init() }.pbi_status == libc::SZOMB
    }

    /// Elsewhere only the session leader's process group is known.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    pub fn members(_session: Pid) -> Vec<Pid> {
        Vec::new()
    }
}
