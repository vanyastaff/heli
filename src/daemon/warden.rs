//! The pool's warden, on Unix: a small process each daemon starts, which
//! outlives it to remove the FIFO it leaves behind (docs/host-pool.md,
//! "Lifetime").
//!
//! A FIFO's path outlives every process that holds it, and once nobody holds
//! it, it has no bytes: a bare cargo pointed at it (`cargo heli setup`)
//! would find no slot and build one job at a time, without a warning. A
//! semaphore has no path and goes with its last handle, so Windows needs no
//! warden.
//!
//! - The daemon starts `cargo-heli daemon --warden` once it has opened the
//!   pool (`start`, Unix only), with the read end of a pipe as its stdin,
//!   and its own process id in `DAEMON_VAR`. The daemon alone holds the write end,
//!   which no run inherits (close-on-exec). However the daemon ends (it
//!   retires, is killed, crashes), the warden then reads the end of its
//!   input.
//! - The end of its input comes as the kernel closes the daemon's files, one
//!   by one: the pool's lock may still be held then, as by a daemon that
//!   owns the pool. So the warden waits, at most `EXIT_WAIT` (Unix only),
//!   until the daemon is no longer its parent: its process is gone then, with every
//!   file it held.
//! - The warden leads a process group of its own, so a signal to the
//!   daemon's group does not take it along, and keeps no stdout or stderr.
//! - Then it removes the FIFO as soon as no process holds it, and exits
//!   (`crate::pool::remove_when_released`). While some process holds it (a
//!   cargo still building), the pool lives on, as a semaphore would. Once a
//!   daemon owns the pool again, the warden exits and leaves it alone.
//! - A warden killed together with its daemon leaves the FIFO behind. The
//!   next daemon fills it again; until then, a bare cargo builds one job at
//!   a time.

/// The variable that names the warden's daemon, by its process id.
#[cfg(unix)]
const DAEMON_VAR: &str = "HELI_WARDEN_OF";

/// The longest a warden waits for its daemon's process to be gone once its
/// input has ended.
#[cfg(unix)]
const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// `cargo-heli daemon --warden`: waits until the daemon at the other end of
/// stdin is gone, then removes its pool once nobody holds it.
pub fn main() -> ! {
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    #[cfg(unix)]
    {
        wait_for_exit();
        if let Ok(locations) = crate::locations::Locations::resolve() {
            crate::pool::remove_when_released(&locations.pool);
        }
    }
    std::process::exit(0)
}

/// Waits, at most [`EXIT_WAIT`], until the daemon that [`DAEMON_VAR`] names
/// is no longer this process's parent: once a process is gone, its children
/// have another parent, and every file it held is closed, the pool's lock
/// with them.
#[cfg(unix)]
fn wait_for_exit() {
    let daemon = std::env::var(DAEMON_VAR)
        .ok()
        .and_then(|pid| pid.parse().ok())
        .and_then(rustix::process::Pid::from_raw);
    let Some(daemon) = daemon else {
        return;
    };
    let deadline = std::time::Instant::now() + EXIT_WAIT;
    while rustix::process::getppid() == Some(daemon) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Starts the warden of the pool in `locations`, and returns the end of the
/// pipe whose closing tells it the daemon is gone (the daemon keeps it
/// until it exits) and the warden's process id.
#[cfg(unix)]
pub fn start(
    locations: &crate::locations::Locations,
) -> std::io::Result<(std::io::PipeWriter, u32)> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use crate::locations::POOL_VAR;
    use crate::nesting;

    let (reader, writer) = std::io::pipe()?;
    let mut warden = Command::new(std::env::current_exe()?)
        .args(["daemon", "--warden"])
        .env(POOL_VAR, &locations.pool)
        .env(DAEMON_VAR, std::process::id().to_string())
        .env(nesting::VAR, (nesting::current() + 1).to_string())
        .current_dir(&locations.state_dir)
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let pid = warden.id();
    // Reaped here should it exit before the daemon; it normally outlives it.
    std::thread::spawn(move || warden.wait());
    Ok((writer, pid))
}
