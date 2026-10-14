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
//! **Accounting.** The pool keeps the slots the memory governor withholds
//! ([`Pool::withhold`]), and adjusts its free slots to its [`target`]: a
//! top-up raises them to it once nobody can still give a slot back
//! ([`Pool::top_up`]), a drain lowers them to it at any time
//! ([`Pool::drain`]). Whether another process holds the pool open is
//! [`Pool::held_by_others`].
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
use std::sync::{Mutex, MutexGuard, PoisonError};

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
    /// The slots the memory governor holds out of the pool.
    withheld: Mutex<u32>,
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
        let pool = Self {
            jobserver,
            withheld: Mutex::new(0),
        };
        Ok((pool, opened))
    }

    /// The value of `CARGO_MAKEFLAGS` that points a cargo at this pool.
    pub fn makeflags(&self) -> OsString {
        makeflags_for(self.jobserver.auth()).into()
    }

    /// The slots, as counted now.
    pub fn status(&self) -> status::Pool {
        let withheld = *self.withheld();
        let slots = self.jobserver.slots();
        // A count that cannot be read shows as no free slot.
        let free = self.jobserver.free().unwrap_or(0);
        status::Pool {
            slots,
            free,
            withheld,
            in_use: slots.saturating_sub(free).saturating_sub(withheld),
        }
    }

    /// Takes one free slot out of the pool and withholds it, unless that
    /// would leave the pool fewer than one slot. Returns the slots withheld
    /// after it, or `None` when it took none (no slot was free).
    pub fn withhold(&self) -> io::Result<Option<u32>> {
        let mut withheld = self.withheld();
        if *withheld + 1 >= self.jobserver.slots() || self.jobserver.take(1)? == 0 {
            return Ok(None);
        }
        *withheld += 1;
        Ok(Some(*withheld))
    }

    /// Gives every withheld slot back to the pool, and returns how many
    /// there were. A slot the pool has no room for (it holds a surplus
    /// already) is not given.
    pub fn return_withheld(&self) -> io::Result<u32> {
        let mut withheld = self.withheld();
        let returned = *withheld;
        self.jobserver.give(returned)?;
        *withheld = 0;
        Ok(returned)
    }

    /// Raises the free slots to the [`target`], `lent` being the slots lent
    /// to run pools; `None` when they are not below it. Only safe when no
    /// process holds a slot it will give back (the governor's quiescence).
    pub fn top_up(&self, lent: u32) -> io::Result<Option<Adjustment>> {
        let withheld = self.withheld();
        let target = target(self.jobserver.slots(), *withheld, lent);
        let free = self.jobserver.free()?;
        if free >= target {
            return Ok(None);
        }
        let given = self.jobserver.give(target - free)?;
        Ok((given > 0).then_some(Adjustment {
            slots: given,
            free: free + given,
            target,
        }))
    }

    /// Takes the free slots above the [`target`] out of the pool, `lent`
    /// being the slots lent to run pools; `None` when there are none. Safe
    /// at any time: such a slot is a surplus, whoever gave it.
    pub fn drain(&self, lent: u32) -> io::Result<Option<Adjustment>> {
        let withheld = self.withheld();
        let target = target(self.jobserver.slots(), *withheld, lent);
        let free = self.jobserver.free()?;
        if free <= target {
            return Ok(None);
        }
        let taken = self.jobserver.take(free - target)?;
        Ok((taken > 0).then_some(Adjustment {
            slots: taken,
            free: free - taken,
            target,
        }))
    }

    /// Whether a process other than this one may hold the pool open, and so
    /// may hold slots it will give back. Where it cannot be known, it may.
    pub fn held_by_others(&self) -> bool {
        self.jobserver.held_by_others().unwrap_or(true)
    }

    fn withheld(&self) -> MutexGuard<'_, u32> {
        self.withheld.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The host pool's target of free slots (DESIGN.md §4, "Bounded top-up"):
/// its `slots` (N), less the slots the memory governor has `withheld`, less
/// the slots `lent` to run pools (Σ lent). A top-up raises the free slots to
/// it, never above.
///
/// For a broker that lends slots: count a loan after taking its slot from
/// the pool, and stop counting it before giving the slot back, so that the
/// free slots never stand above the target meanwhile.
pub fn target(slots: u32, withheld: u32, lent: u32) -> u32 {
    slots.saturating_sub(withheld).saturating_sub(lent)
}

/// Slots given to the pool or taken out of it, to meet its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adjustment {
    /// How many.
    pub slots: u32,
    /// The free slots after it.
    pub free: u32,
    pub target: u32,
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
