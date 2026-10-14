//! The pool's warden, on Unix: a small process each daemon starts, which
//! outlives it to remove the FIFO it leaves behind (DESIGN.md §4,
//! "Lifetime").
//!
//! A FIFO's path outlives every process that holds it, and once nobody holds
//! it, it has no bytes: a bare cargo pointed at it (`cargo heli setup`)
//! would find no slot and build one job at a time, without a warning. A
//! semaphore has no path and goes with its last handle, so Windows needs no
//! warden.
//!
//! - The daemon starts `cargo-heli daemon --warden` once it has opened the
//!   pool ([`start`]), with the read end of a pipe as its stdin. The daemon
//!   alone holds the write end, which no run inherits (close-on-exec).
//!   However the daemon ends (it retires, is killed, crashes), the warden
//!   then reads the end of its input.
//! - The warden leads a process group of its own, so a signal to the
//!   daemon's group does not take it along, and keeps no stdout or stderr.
//! - Then it removes the FIFO as soon as no process holds it, and exits
//!   (`crate::pool::remove_when_released`). While some process holds it (a
//!   cargo still building), the pool lives on, as a semaphore would. Once a
//!   daemon owns the pool again, the warden exits and leaves it alone.
//! - A warden killed together with its daemon leaves the FIFO behind. The
//!   next daemon fills it again; until then, a bare cargo builds one job at
//!   a time.

/// `cargo-heli daemon --warden`: waits until the daemon at the other end of
/// stdin is gone, then removes its pool once nobody holds it.
pub fn main() -> ! {
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    #[cfg(unix)]
    if let Ok(locations) = crate::locations::Locations::resolve() {
        crate::pool::remove_when_released(&locations.pool);
    }
    std::process::exit(0)
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
