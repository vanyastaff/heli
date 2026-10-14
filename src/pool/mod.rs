//! The host pool (DESIGN.md §4): one jobserver with N slots, which every run
//! and every cargo, rustc and build script it starts draws from, through
//! `CARGO_MAKEFLAGS` ([`Pool::makeflags`]).
//!
//! - **Windows**: a named semaphore in the logon session's namespace, with
//!   maximum count N; its count is the free slots.
//! - **Unix**: a FIFO with mode 0600 in a directory of mode 0700, holding one
//!   byte per free slot.
//!
//! A slot is taken by a process the jobserver protocol starts for each job
//! beyond its first: each process has one implicit job outside the pool, so
//! no run ever waits for another to finish, and k processes run at most
//! N + k jobs.
//!
//! **Adoption.** The daemon keeps the pool open for its whole life. A
//! restarted daemon adopts the pool it finds still held by some process (a
//! cargo still building): that cargo gives its slots back to the same pool,
//! so a restart never doubles it. A semaphore lives while any handle to it
//! is open, and a FIFO keeps its bytes only while some process holds it
//! open; one that nobody held has none, and the daemon fills it again.
//!
//! **Lifetime.** A pool lives while some process holds it, and no longer:
//! once its daemon is gone and nobody holds it, a bare cargo pointed at it
//! cannot open it, warns, and builds with a pool of its own. A semaphore
//! goes with its last handle; a FIFO's path stays, so the daemon's warden
//! removes it ([`remove_when_released`]).
//!
//! The counts are snapshots: other processes take and return slots at any
//! moment.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

use std::ffi::{OsStr, OsString};
use std::io;

use crate::status;

/// The most slots a pool may have: a FIFO's whole capacity must fit them.
pub const MAX_SLOTS: u32 = 1024;

/// The default size: the number of logical CPUs heli may use, as for
/// cargo's own default `-j`.
pub fn default_slots() -> u32 {
    std::thread::available_parallelism()
        .map_or(1, |cpus| u32::try_from(cpus.get()).unwrap_or(MAX_SLOTS))
        .min(MAX_SLOTS)
}

/// The host pool, held open.
pub struct Pool {
    jobserver: platform::Jobserver,
}

/// How [`Pool::open`] found the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// It made a new pool, with every slot free.
    Created,
    /// A process still held the pool, which it now holds too, as it was.
    Adopted,
}

impl Pool {
    /// Adopts the pool `name` (a semaphore name on Windows, a FIFO path
    /// elsewhere) if some process still holds it, else creates it with
    /// `slots` free slots.
    pub fn open(name: &OsStr, slots: u32) -> io::Result<(Self, Opened)> {
        if !(1..=MAX_SLOTS).contains(&slots) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a pool has 1 to {MAX_SLOTS} slots, not {slots}"),
            ));
        }
        let (jobserver, opened) = platform::Jobserver::open(name, slots)?;
        Ok((Self { jobserver }, opened))
    }

    /// The value of `CARGO_MAKEFLAGS` that points a cargo at this pool.
    pub fn makeflags(&self) -> OsString {
        makeflags_for(self.jobserver.auth()).into()
    }

    /// The slots, as counted now.
    pub fn status(&self) -> status::Pool {
        let slots = self.jobserver.slots();
        // A count that cannot be read shows as no free slot.
        let free = self.jobserver.free().unwrap_or(0);
        // The memory governor will withhold slots; nothing does yet.
        let withheld = 0;
        status::Pool {
            slots,
            free,
            withheld,
            in_use: slots.saturating_sub(free).saturating_sub(withheld),
        }
    }
}

/// Once the daemon that owned the pool `name` is gone, removes the pool as
/// soon as no process holds it, unless a daemon owns it again by then: the
/// work of the pool's warden (`crate::daemon::warden`). Only a FIFO needs
/// it; a semaphore goes with its last handle.
#[cfg(unix)]
pub fn remove_when_released(name: &OsStr) {
    platform::remove_when_released(std::path::Path::new(name));
}

/// The value of `CARGO_MAKEFLAGS` that points a cargo at the pool `name`
/// (as [`Pool::open`] takes it), whether or not the pool exists now; the
/// error says why no cargo could find a pool of that name.
pub fn makeflags(name: &OsStr) -> io::Result<String> {
    Ok(makeflags_for(&platform::auth(name)?))
}

/// `CARGO_MAKEFLAGS` for the jobserver that `--jobserver-auth=` names `auth`.
fn makeflags_for(auth: &str) -> String {
    format!("-j --jobserver-auth={auth}")
}

/// Whether `name` can travel in `CARGO_MAKEFLAGS`: the jobserver protocol
/// ends the value at the first space, and cargo reads it as UTF-8.
fn check_name(name: &OsStr) -> io::Result<&str> {
    match name.to_str() {
        Some(text) if !text.is_empty() && !text.chars().any(char::is_whitespace) => Ok(text),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} cannot name a jobserver: it must be non-empty Unicode without spaces",
                name.display()
            ),
        )),
    }
}
