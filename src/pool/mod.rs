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
//! The counts are snapshots: other processes take and return slots at any
//! moment.
//!
//! **Run pools** ([`RunPool`]). Each heli run draws from a jobserver of its
//! own, named after the host pool and the run (`<pool>-r17`), which starts
//! with no free slot. The [`broker`] lends it slots from the host pool and
//! takes them back.

pub mod broker;

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

pub use platform::Waiter;

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
        makeflags(&self.jobserver)
    }

    /// N.
    pub fn slots(&self) -> u32 {
        self.jobserver.slots()
    }

    /// The free slots, as counted now.
    pub fn free(&self) -> io::Result<u32> {
        self.jobserver.free()
    }

    /// Takes a free slot if there is one, without waiting.
    pub fn try_take(&self) -> io::Result<bool> {
        self.jobserver.try_take()
    }

    /// Gives a slot back; `false` when the pool is already full (on
    /// Windows, at its maximum count), and the slot is dropped.
    pub fn give(&self) -> io::Result<bool> {
        self.jobserver.give()
    }

    /// Creates the run pool of run `run`: `<this pool's name>-<run>`, with
    /// no free slot and room for all of this pool's.
    pub fn run_pool(&self, run: &str) -> io::Result<RunPool> {
        let mut name = self.jobserver.name().to_owned();
        name.push(format!("-{run}"));
        let jobserver = platform::Jobserver::create_empty(&name, self.slots())?;
        Ok(RunPool { jobserver })
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

/// A run's own jobserver, which only the [`broker`] fills. Dropping it
/// removes its name (on Unix, its path); processes that still hold it keep
/// it.
pub struct RunPool {
    jobserver: platform::Jobserver,
}

impl RunPool {
    /// The value of `CARGO_MAKEFLAGS` that points the run's processes here.
    pub fn makeflags(&self) -> OsString {
        makeflags(&self.jobserver)
    }

    /// The free slots, as counted now.
    pub fn free(&self) -> io::Result<u32> {
        self.jobserver.free()
    }

    /// Takes a free slot if there is one, without waiting.
    pub fn try_take(&self) -> io::Result<bool> {
        self.jobserver.try_take()
    }

    /// Puts a slot in; `false` when the pool is already full, and the slot
    /// was not put in.
    pub fn give(&self) -> io::Result<bool> {
        self.jobserver.give()
    }

    /// A waiter that takes slots in line with the run's processes.
    pub fn waiter(&self) -> io::Result<Waiter> {
        self.jobserver.waiter()
    }
}

impl Drop for RunPool {
    fn drop(&mut self) {
        self.jobserver.remove();
    }
}

/// `CARGO_MAKEFLAGS` for `jobserver`. heli sets no job count: under a
/// jobserver, cargo ignores one.
fn makeflags(jobserver: &platform::Jobserver) -> OsString {
    format!("-j --jobserver-auth={}", jobserver.auth()).into()
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
