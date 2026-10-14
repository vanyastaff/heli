//! The broker (DESIGN.md §4, "Run pools"): each heli run draws from a run
//! pool of its own, and the broker moves slots between the host pool and
//! the run pools, every [`TICK`]:
//!
//! - **Lend.** A run whose pool has no free slot may be waiting for one: it
//!   gets one from the host pool, the waiting run with the fewest slots in
//!   use first.
//! - **Return.** Free slots in a run pool beyond one go back to the host
//!   pool. The one left goes back too when the host pool has none free and
//!   a run that waits uses fewer slots than this one; an idle run (using
//!   none) keeps it, for the next process that asks.
//! - **Share.** A run's share is the capacity (the host pool's free slots
//!   plus those the active runs hold) divided by the active runs. While
//!   another run waits, a run using its share or more gets no new slot. A
//!   run alone can borrow every slot.
//! - **Reclaim.** While the host pool has no free slot and a run waits, a
//!   run using more than its share, and at least two slots more than the
//!   waiting run with the fewest, gives slots back as its processes finish
//!   with them. A process waiting for a slot from a run pool takes the next
//!   one its run gives back, so the slots of a run with work pending are
//!   never free there: the broker waits for one in line with them (a
//!   [`Waiter`] in a thread of its own) and returns it to the host pool,
//!   which lends it on. Nothing running is interrupted.
//! - **Exact accounting.** A run uses the slots lent to it less those free
//!   in its pool. When it ends, everything lent to it goes back to the host
//!   pool, whatever its processes still held ([`Broker::close`]).
//!
//! A run is *waiting* when its pool has no free slot: the broker cannot see
//! the processes blocked on it, so an empty pool counts as a run asking for
//! more. A run is *active* when it uses a slot or waits; a run that only
//! keeps a free slot in its pool is idle and does not count for shares.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::{Pool, RunPool, Waiter};
use crate::daemon::registry::RunId;

/// How often the broker looks at the pools.
pub const TICK: Duration = Duration::from_millis(20);

/// How many waits for a lent slot a run's [`Loans`] keeps, for the median.
const KEPT_WAITS: usize = 1 << 16;

pub struct Broker {
    host: Arc<Pool>,
    state: Mutex<State>,
    /// Signalled whenever the broker has moved slots, or a run came.
    changed: Condvar,
}

#[derive(Default)]
struct State {
    runs: BTreeMap<RunId, Run>,
}

/// What the broker knows of a run.
struct Run {
    /// `None` when it could not be created: the run then draws from the
    /// host pool directly.
    pool: Option<RunPool>,
    /// Slots taken from the host pool and put in the run pool, whether its
    /// processes hold them or they are free there.
    lent: u32,
    /// The run pool's free slots, as last counted.
    free: u32,
    /// When the run pool was last seen with a free slot: a wait for a lent
    /// slot began no earlier.
    last_free: Instant,
    /// The waiter that takes slots back, once one was wanted.
    reclaim: Option<Reclaim>,
    /// A waiter could not be made: slots are not taken back.
    unreclaimable: bool,
    loans: Loans,
}

/// The broker's waiter on a run pool, which takes slots back from the run.
struct Reclaim {
    waiter: Arc<Waiter>,
    /// Whether the broker wants a slot back now.
    wanted: bool,
    /// Whether the waiter is waiting.
    blocked: bool,
}

/// How a run's loans went, for the daemon's log.
#[derive(Debug, Default)]
pub struct Loans {
    /// Slots lent to the run pool.
    pub count: u32,
    /// Slots taken back from it while it had work pending.
    pub reclaimed: u32,
    /// The most slots the run was seen using.
    pub most: u32,
    /// How long the run waited for each lent slot, at most: from when its
    /// pool was last seen with a free slot. The first [`KEPT_WAITS`].
    waits: Vec<Duration>,
    /// The longest of all those waits.
    longest: Duration,
}

/// A run's use of the pool, for `status`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// The slots it uses.
    pub slots: u32,
    /// Its share of the capacity.
    pub share: f64,
}

impl Broker {
    /// A broker lending the slots of `host`. It works in a thread of its
    /// own for as long as the process lives.
    pub fn start(host: Arc<Pool>) -> Arc<Self> {
        let broker = Arc::new(Self {
            host,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker = Arc::clone(&broker);
        std::thread::spawn(move || worker.work());
        broker
    }

    /// Gives run `id` a run pool, and returns the `CARGO_MAKEFLAGS` its
    /// processes get. When the run pool cannot be created, the run draws
    /// from the host pool, and the error says why.
    pub fn open(self: &Arc<Self>, id: RunId) -> (OsString, Option<io::Error>) {
        let (pool, makeflags, error) = match self.host.run_pool(&id.to_string()) {
            Ok(pool) => {
                let makeflags = pool.makeflags();
                (Some(pool), makeflags, None)
            }
            Err(error) => (None, self.host.makeflags(), Some(error)),
        };
        let mut state = self.lock();
        state.runs.insert(
            id,
            Run {
                pool,
                lent: 0,
                free: 0,
                last_free: Instant::now(),
                reclaim: None,
                unreclaimable: false,
                loans: Loans::default(),
            },
        );
        self.balance(&mut state);
        drop(state);
        self.changed.notify_all();
        (makeflags, error)
    }

    /// Run `id` has ended: everything lent to it goes back to the host
    /// pool at once, and its pool is removed. Returns how its loans went.
    pub fn close(self: &Arc<Self>, id: RunId) -> Option<Loans> {
        let mut state = self.lock();
        let run = state.runs.remove(&id)?;
        for _ in 0..run.lent {
            self.give_host();
        }
        // The waiter, if it waits, finds the run gone.
        if let Some(reclaim) = &run.reclaim
            && reclaim.blocked
        {
            reclaim.waiter.wake();
        }
        self.balance(&mut state);
        drop(state);
        self.changed.notify_all();
        Some(run.loans)
    }

    /// Each run's slots and share, counted now.
    pub fn usage(&self) -> BTreeMap<RunId, Usage> {
        let mut state = self.lock();
        state.observe(Instant::now());
        let shares = self.shares(&state);
        state
            .runs
            .iter()
            .map(|(&id, run)| {
                let usage = Usage {
                    slots: run.in_use(),
                    share: shares.of(run),
                };
                (id, usage)
            })
            .collect()
    }

    /// Slots the broker has taken out of the host pool and not yet given
    /// back: those lent to run pools. The host pool's top-up target leaves
    /// them out (DESIGN.md §4, "Bounded top-up").
    #[allow(dead_code, reason = "the top-up target (#6) reads it")]
    pub fn lent(&self) -> u32 {
        self.lock().runs.values().map(Run::holdings).sum()
    }

    /// Balances the pools every [`TICK`] while there are runs.
    fn work(self: Arc<Self>) {
        loop {
            let mut state = self.lock();
            while state.runs.is_empty() {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            self.balance(&mut state);
            drop(state);
            self.changed.notify_all();
            std::thread::sleep(TICK);
        }
    }

    /// Looks at every pool and moves slots as the module docs say.
    fn balance(self: &Arc<Self>, state: &mut State) {
        let now = Instant::now();
        state.observe(now);
        for run in state.runs.values_mut() {
            while run.free > 1 && run.lent > 0 && run.take_free() {
                self.give_host();
            }
        }
        if self.host.free().unwrap_or(0) == 0
            && let Some(fewest) = state.fewest_waiting()
        {
            for run in state.runs.values_mut() {
                if !run.waiting() && run.in_use() > fewest && run.take_free() {
                    self.give_host();
                }
            }
        }
        let shares = self.shares(state);
        while let Some(id) = state.next_loan(&shares) {
            if !self.host.try_take().unwrap_or(false) {
                break;
            }
            let run = state.runs.get_mut(&id).expect("BUG: a run just chosen");
            if !run.lend(now) {
                self.give_host();
                break;
            }
        }
        self.reclaim(state);
    }

    /// Decides which runs give slots back (see the module docs), and starts
    /// or stops their waiters.
    fn reclaim(self: &Arc<Self>, state: &mut State) {
        let shares = self.shares(state);
        let saturated = self.host.free().unwrap_or(0) == 0;
        let fewest = state.fewest_waiting().filter(|_| saturated);
        let wanted: Vec<(RunId, bool)> = state
            .runs
            .iter()
            .map(|(&id, run)| {
                let in_use = run.in_use();
                let wanted = fewest.is_some_and(|fewest| {
                    in_use > 0 && in_use >= fewest + 2 && f64::from(in_use) > shares.of(run)
                });
                (id, wanted)
            })
            .collect();
        for (id, wanted) in wanted {
            let run = state.runs.get_mut(&id).expect("BUG: a run just listed");
            match &mut run.reclaim {
                Some(reclaim) => {
                    if reclaim.wanted && !wanted {
                        reclaim.waiter.cancel();
                    }
                    reclaim.wanted = wanted;
                }
                None if wanted && !run.unreclaimable => {
                    match run.pool.as_ref().map(RunPool::waiter) {
                        Some(Ok(waiter)) => {
                            let waiter = Arc::new(waiter);
                            run.reclaim = Some(Reclaim {
                                waiter: Arc::clone(&waiter),
                                wanted: true,
                                blocked: false,
                            });
                            let broker = Arc::clone(self);
                            std::thread::spawn(move || broker.take_back(id, &waiter));
                        }
                        _ => run.unreclaimable = true,
                    }
                }
                None => {}
            }
        }
    }

    /// Takes slots back from run `id` with `waiter` whenever the broker
    /// wants one, until the run ends. A slot it takes goes back to the host
    /// pool and is lent on at once; one taken when none is wanted any more
    /// goes back to the run pool.
    fn take_back(self: Arc<Self>, id: RunId, waiter: &Waiter) {
        let mut state = self.lock();
        loop {
            loop {
                let Some(reclaim) = state.runs.get_mut(&id).and_then(|run| run.reclaim.as_mut())
                else {
                    return;
                };
                if reclaim.wanted {
                    // Armed under the lock, so a cancel is never lost.
                    waiter.arm();
                    reclaim.blocked = true;
                    break;
                }
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            drop(state);
            let taken = waiter.take();
            state = self.lock();
            // Gone, the run took its pool with it, and the slot.
            let Some(run) = state.runs.get_mut(&id) else {
                return;
            };
            let Some(reclaim) = run.reclaim.as_mut() else {
                return;
            };
            reclaim.blocked = false;
            match taken {
                Ok(false) => {}
                Ok(true) if reclaim.wanted => {
                    run.lent = run.lent.saturating_sub(1);
                    run.loans.reclaimed += 1;
                    self.give_host();
                    self.balance(&mut state);
                    self.changed.notify_all();
                }
                Ok(true) => {
                    if !run
                        .pool
                        .as_ref()
                        .is_some_and(|pool| pool.give().unwrap_or(false))
                    {
                        run.lent = run.lent.saturating_sub(1);
                        self.give_host();
                    }
                }
                Err(_) => {
                    run.reclaim = None;
                    run.unreclaimable = true;
                    return;
                }
            }
        }
    }

    /// The capacity and the active runs, as counted last.
    fn shares(&self, state: &State) -> Shares {
        let free = self.host.free().unwrap_or(0);
        let active = state.runs.values().filter(|run| run.active());
        Shares {
            capacity: free + active.clone().map(Run::holdings).sum::<u32>(),
            active: u32::try_from(active.count()).unwrap_or(u32::MAX),
        }
    }

    /// Gives a slot back to the host pool. A pool already full drops it: it
    /// never grows beyond N.
    fn give_host(&self) {
        let _ = self.host.give();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Counts every run pool's free slots.
    fn observe(&mut self, now: Instant) {
        for run in self.runs.values_mut() {
            run.observe(now);
        }
    }

    /// The fewest slots a waiting run uses.
    fn fewest_waiting(&self) -> Option<u32> {
        self.runs
            .values()
            .filter(|run| run.waiting())
            .map(Run::in_use)
            .min()
    }

    /// The run the next free slot of the host pool goes to: the waiting run
    /// with the fewest slots in use, the oldest first, unless it uses its
    /// share already while another run waits.
    fn next_loan(&self, shares: &Shares) -> Option<RunId> {
        let waiting: Vec<(RunId, &Run)> = self
            .runs
            .iter()
            .filter(|(_, run)| run.waiting())
            .map(|(&id, run)| (id, run))
            .collect();
        let alone = waiting.len() == 1;
        waiting
            .into_iter()
            .filter(|(_, run)| alone || f64::from(run.in_use()) < shares.of(run))
            .min_by_key(|&(id, run)| (run.in_use(), id))
            .map(|(id, _)| id)
    }
}

impl Run {
    /// Counts the run pool's free slots.
    fn observe(&mut self, now: Instant) {
        let Some(pool) = &self.pool else {
            return;
        };
        self.free = pool.free().unwrap_or(0);
        if self.free > 0 {
            self.last_free = now;
        }
        self.loans.most = self.loans.most.max(self.in_use());
    }

    /// Takes a free slot out of the run pool.
    fn take_free(&mut self) -> bool {
        let taken = self
            .pool
            .as_ref()
            .is_some_and(|pool| pool.try_take().unwrap_or(false));
        if taken {
            self.lent = self.lent.saturating_sub(1);
            self.free = self.free.saturating_sub(1);
        }
        taken
    }

    /// Puts a slot taken from the host pool in the run pool; `false` when it
    /// could not, and the slot is still the caller's.
    fn lend(&mut self, now: Instant) -> bool {
        let Some(pool) = &self.pool else {
            return false;
        };
        if !pool.give().unwrap_or(false) {
            return false;
        }
        self.lent += 1;
        self.loans
            .record(now.saturating_duration_since(self.last_free));
        // A process waiting on a semaphore takes the slot as it is put in;
        // counted again at once, the run still waits if it did.
        self.observe(now);
        true
    }

    /// The slots the run uses: lent, and not free in its pool.
    fn in_use(&self) -> u32 {
        self.lent.saturating_sub(self.free)
    }

    /// The slots taken from the host pool for the run.
    fn holdings(&self) -> u32 {
        self.lent
    }

    /// Whether the run may be waiting for a slot.
    fn waiting(&self) -> bool {
        self.pool.is_some() && self.free == 0
    }

    /// Whether the run uses or wants slots.
    fn active(&self) -> bool {
        self.in_use() > 0 || self.waiting()
    }
}

/// The capacity of the host pool for heli runs, and how many runs share it.
struct Shares {
    capacity: u32,
    active: u32,
}

impl Shares {
    /// The share of `run`: the capacity divided by the active runs, `run`
    /// among them.
    fn of(&self, run: &Run) -> f64 {
        let runs = self.active + u32::from(!run.active());
        f64::from(self.capacity) / f64::from(runs.max(1))
    }
}

impl Loans {
    fn record(&mut self, wait: Duration) {
        self.count += 1;
        self.longest = self.longest.max(wait);
        if self.waits.len() < KEPT_WAITS {
            self.waits.push(wait);
        }
    }

    /// The median wait for a lent slot, and the longest.
    pub fn waits(&self) -> Option<(Duration, Duration)> {
        let mut waits = self.waits.clone();
        waits.sort_unstable();
        let median = *waits.get(waits.len() / 2)?;
        Some((median, self.longest))
    }
}
