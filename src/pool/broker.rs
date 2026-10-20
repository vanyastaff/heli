//! The broker (docs/host-pool.md, "Run pools"): each heli run draws from a run
//! pool of its own, and the broker moves slots between the host pool and
//! the run pools, every [`TICK`]:
//!
//! - **Lend.** A run whose pool has no free slot may be waiting for one: it
//!   gets one from the host pool, the waiting run with the fewest slots in
//!   use for its weight first.
//! - **Return.** Free slots in a run pool beyond one go back to the host
//!   pool once they have stayed free for a [`TICK`]: until then, they may
//!   be slots that a run with work pending gave back and that its processes
//!   waiting on the pool have not yet woken to take (a process blocked on a
//!   FIFO takes a byte only once the scheduler runs it). The one left goes
//!   back too when the host pool has none free and a run that waits would
//!   still use fewer slots for its weight with it than this one does; an
//!   idle run (using none) keeps it, for the next process that asks.
//! - **Weight.** A run weighs what its priority says
//!   ([`Priority::weight`]): interactive 4, normal 1, background ¼. Runs
//!   are compared by the slots they use *for their weight*, `in_use / w`.
//! - **Share.** A run's share is the capacity (the host pool's free slots
//!   plus those the active runs hold) times its weight over the sum of the
//!   active runs' weights: with equal weights, the capacity divided by the
//!   active runs. While another run waits, a run using its share or more
//!   gets no new slot. A run alone can borrow every slot.
//! - **Reclaim.** While the host pool has no free slot and a run waits, a
//!   run using more than its share, which with one slot fewer would still
//!   use as many for its weight as the neediest waiting run would with one
//!   more (with equal weights: at least two slots more than it), gives
//!   slots back as its processes finish with them. A process waiting for a slot from a run pool takes the next
//!   one its run gives back, so the slots of a run with work pending are
//!   never free there: the broker waits for one in line with them (a
//!   [`Waiter`] in a thread of its own) and returns it to the host pool,
//!   which lends it on, if a slot is still wanted back by the count made
//!   as it gets one; else it goes back to the run's pool (see
//!   [`Broker::take_back`]). Nothing running is interrupted.
//! - **Exact accounting.** A run uses the slots lent to it less those free
//!   in its pool, plus its test slots. When it ends, everything lent to it
//!   goes back to the host pool, whatever its processes still held
//!   ([`Broker::close`]).
//! - **Test slots.** Before its test phase, a test run asks for K slots
//!   ([`Broker::take_test_slots`]) and waits for them at most a few
//!   seconds. Free slots in its own pool count first; it then waits like
//!   any run that waits, the fewest in use for its weight first. The daemon holds the
//!   slots it got for the test threads until the phase ends.
//!
//! A run is *waiting* when its pool has no free slot, or it waits for test
//! slots: the broker cannot see the processes blocked on a pool, so an empty
//! pool counts as a run asking for more. A run is *active* when it uses a
//! slot or waits; a run that only keeps a free slot in its pool is idle and
//! does not count for shares.
//!
//! **Saturation** (docs/host-pool.md, "Saturation"): the pool is saturated
//! while the host pool has no free slot, two runs or more are active and
//! one of them waits; a spell of it ends once it has not been for [`CALM`].
//! Nothing changes for that: no run is held back. The
//! broker only counts it, for `status` and the runs' stderr
//! ([`Broker::pressure`], [`Broker::crowding`]), and reports each spell
//! that lasted [`SATURATED_AFTER`] or more as it ends ([`Spell`]). It also
//! keeps every run's waits for a lent slot over the last [`WINDOW`].
//!
//! The broker counts every slot it takes out of the host pool, and stops
//! counting it as it gives it back, each as one step under the host pool's
//! lock ([`Pool::lend`], [`Pool::repay`]): [`Broker::lent`], which the host
//! pool's top-up and drain read under that lock, is always exact.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::{Pool, RunPool, Waiter};
use crate::priority::Priority;
use crate::run_id::RunId;

/// How often the broker looks at the pools.
pub const TICK: Duration = Duration::from_millis(20);

/// How many waits for a lent slot a run's [`Loans`] keeps, for the median.
const KEPT_WAITS: usize = 1 << 16;

/// How long the waits for a lent slot that `status` counts go back.
pub const WINDOW: Duration = Duration::from_secs(600);

/// How many of those waits the broker keeps, at most.
const KEPT_RECENT: usize = 1 << 16;

/// How long the pool must have been saturated before a run that joins it is
/// told, and before a spell of it is reported. Every build fills the pool
/// for moments as its units start and end; a few seconds of it is no news.
pub const SATURATED_AFTER: Duration = Duration::from_secs(5);

/// How long the pool must stay unsaturated for a spell to end while two
/// runs or more are active. A run that gives a slot back and asks for the
/// next one leaves it free for a tick or two; that is no end of the crowd.
const CALM: Duration = Duration::from_secs(1);

pub struct Broker {
    host: Arc<Pool>,
    /// Told of each spell of saturation that lasted [`SATURATED_AFTER`], as
    /// it ends.
    report: Box<dyn Fn(&Spell) + Send + Sync>,
    /// The slots taken out of the host pool: every run's holdings.
    lent: AtomicU32,
    state: Mutex<State>,
    /// Signalled whenever the broker has moved slots, or a run came.
    changed: Condvar,
}

#[derive(Default)]
struct State {
    runs: BTreeMap<RunId, Run>,
    /// The spell of saturation going on, if one is.
    saturated: Option<Spell>,
    /// Since when the pool has not been saturated, while a spell goes on:
    /// the spell ends after [`CALM`] of it.
    calm_since: Option<Instant>,
    /// Every run's waits for a lent slot over the last [`WINDOW`], each
    /// with when it ended, oldest first.
    recent: VecDeque<(Instant, Duration)>,
}

/// A spell of saturation (see the module docs).
#[derive(Debug, Clone)]
pub struct Spell {
    /// When it began.
    pub since: Instant,
    /// When it began, in seconds since 1970.
    pub since_unix: u64,
    /// How long it lasted: so far, while it goes on.
    pub lasted: Duration,
    /// The most runs active at once in it.
    pub most_active: u32,
    /// The most runs waiting at once in it.
    pub most_waiting: u32,
}

/// How crowded the pool is, for `status` ([`Broker::pressure`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pressure {
    /// The runs that use slots or wait for one.
    pub active_runs: u32,
    /// The runs that wait for a slot while the pool is saturated; 0 while
    /// it is not.
    pub waiting_runs: u32,
    /// How long the pool has been saturated, while it is.
    pub saturated_for: Option<Duration>,
    /// The median and the 90th percentile of the waits for a lent slot
    /// over the last [`WINDOW`]; `None` without any.
    pub wait_p50: Option<Duration>,
    pub wait_p90: Option<Duration>,
}

/// How crowded the pool is for a run that just joined it
/// ([`Broker::crowding`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Crowding {
    /// The runs that share the pool, the new one among them.
    pub runs: u32,
    /// The run's share of the capacity.
    pub share: f64,
    /// How long the pool has been saturated.
    pub saturated_for: Duration,
}

/// What the broker knows of a run.
struct Run {
    /// Its priority's weight in the shares.
    weight: f64,
    /// `None` when it could not be created: the run then draws from the
    /// host pool directly.
    pool: Option<RunPool>,
    /// Slots taken from the host pool and put in the run pool, whether its
    /// processes hold them or they are free there.
    lent: u32,
    /// The run pool's free slots, as last counted.
    free: u32,
    /// Since when every count has seen more than one free slot in the run
    /// pool: a surplus that has lasted a [`TICK`] goes back (Return).
    surplus_since: Option<Instant>,
    /// When the run pool was last seen with a free slot: a wait for a lent
    /// slot began no earlier.
    last_free: Instant,
    /// Test slots asked for, and held: taken from the host pool, kept by
    /// the daemon for the run's test threads.
    test_wanted: u32,
    test_held: u32,
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
    /// Whether the pool was saturated at some time while the run was open.
    pub saturated: bool,
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
    pub fn start(host: Arc<Pool>, report: Box<dyn Fn(&Spell) + Send + Sync>) -> Arc<Self> {
        let broker = Arc::new(Self {
            host,
            report,
            lent: AtomicU32::new(0),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker = Arc::clone(&broker);
        std::thread::spawn(move || worker.work());
        broker
    }

    /// Gives run `id`, of `priority`, a run pool, and returns the
    /// `CARGO_MAKEFLAGS` its processes get. When the run pool cannot be
    /// created, the run draws from the host pool, and the error says why.
    pub fn open(self: &Arc<Self>, id: RunId, priority: Priority) -> (OsString, Option<io::Error>) {
        let (pool, makeflags, error) = match self.host.run_pool(&id.to_string()) {
            Ok(pool) => {
                let makeflags = pool.makeflags();
                (Some(pool), makeflags, None)
            }
            Err(error) => (None, self.host.makeflags(), Some(error)),
        };
        let mut state = self.lock();
        state.runs.insert(id, Run::new(pool, priority));
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
        for _ in 0..run.holdings() {
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

    /// Run `id`'s share of the capacity, counted now.
    pub fn share(&self, id: RunId) -> f64 {
        let mut state = self.lock();
        state.observe(Instant::now());
        let shares = self.shares(&state);
        state.runs.get(&id).map_or(0.0, |run| shares.of(run))
    }

    /// Weighs run `id` as `priority` from now on: demoted for having no
    /// client, or its own priority again (`crate::daemon::detached`).
    pub fn reweigh(self: &Arc<Self>, id: RunId, priority: Priority) {
        let mut state = self.lock();
        let Some(run) = state.runs.get_mut(&id) else {
            return;
        };
        run.weight = priority.weight();
        self.balance(&mut state);
        drop(state);
        self.changed.notify_all();
    }

    /// How many runs other than `except` may be waiting for a slot, counted
    /// now.
    pub fn waiting(&self, except: RunId) -> usize {
        let mut state = self.lock();
        state.observe(Instant::now());
        state
            .runs
            .iter()
            .filter(|(id, run)| **id != except && run.waiting())
            .count()
    }

    /// Takes up to `wanted` test slots for run `id`, waiting for them at
    /// most `within`, and returns how many it got. The daemon holds them
    /// until [`Broker::release_test_slots`].
    pub fn take_test_slots(self: &Arc<Self>, id: RunId, wanted: u32, within: Duration) -> u32 {
        let deadline = Instant::now() + within;
        let mut state = self.lock();
        if let Some(run) = state.runs.get_mut(&id) {
            run.test_wanted = wanted;
        }
        self.balance(&mut state);
        loop {
            let Some(run) = state.runs.get_mut(&id) else {
                return 0;
            };
            let now = Instant::now();
            if run.test_held >= wanted || now >= deadline {
                run.test_wanted = run.test_held;
                return run.test_held;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Gives run `id`'s test slots back to the host pool.
    pub fn release_test_slots(self: &Arc<Self>, id: RunId) {
        let mut state = self.lock();
        let Some(run) = state.runs.get_mut(&id) else {
            return;
        };
        let held = std::mem::take(&mut run.test_held);
        run.test_wanted = 0;
        for _ in 0..held {
            self.give_host();
        }
        self.balance(&mut state);
        drop(state);
        self.changed.notify_all();
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

    /// How crowded the pool is now.
    pub fn pressure(&self) -> Pressure {
        let mut state = self.lock();
        let now = Instant::now();
        state.observe(now);
        state.forget_waits(now);
        let mut waits: Vec<Duration> = state.recent.iter().map(|&(_, wait)| wait).collect();
        waits.sort_unstable();
        let percentile = |p: usize| {
            waits
                .get((waits.len() * p / 100).min(waits.len().saturating_sub(1)))
                .copied()
        };
        let saturated = state.saturated.as_ref();
        Pressure {
            active_runs: state.active(),
            waiting_runs: if saturated.is_some() {
                state.waiting()
            } else {
                0
            },
            saturated_for: saturated.map(|spell| now.saturating_duration_since(spell.since)),
            wait_p50: percentile(50),
            wait_p90: percentile(90),
        }
    }

    /// How crowded the pool is for run `id`, which just joined it: `None`
    /// unless it has been saturated for [`SATURATED_AFTER`] or more.
    pub fn crowding(&self, id: RunId) -> Option<Crowding> {
        let mut state = self.lock();
        let now = Instant::now();
        state.observe(now);
        let saturated_for = now.saturating_duration_since(state.saturated.as_ref()?.since);
        if saturated_for < SATURATED_AFTER {
            return None;
        }
        let shares = self.shares(&state);
        let run = state.runs.get(&id)?;
        Some(Crowding {
            runs: state.active().max(1),
            share: shares.of(run),
            saturated_for,
        })
    }

    /// Slots the broker has taken out of the host pool and not yet given
    /// back: those lent to run pools, and the test slots held for runs. The
    /// host pool's top-up target leaves them out (docs/host-pool.md, "Bounded
    /// top-up").
    pub fn lent(&self) -> u32 {
        self.lent.load(Ordering::SeqCst)
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
        self.settle(state, now);
    }

    /// Moves slots as the module docs say, by the pools as counted at `now`.
    fn settle(self: &Arc<Self>, state: &mut State, now: Instant) {
        for run in state.runs.values_mut() {
            while run.test_held < run.test_wanted && run.take_free() {
                run.test_held += 1;
            }
            while run.free > 1 && run.lent > 0 && run.surplus_lasted(now) && run.take_free() {
                self.give_host();
            }
        }
        if self.host.free().unwrap_or(0) == 0
            && let Some(neediest) = state.neediest_waiting()
        {
            for run in state.runs.values_mut() {
                if !run.waiting() && yields(run.in_use(), run.weight, neediest) && run.take_free() {
                    self.give_host();
                }
            }
        }
        let shares = self.shares(state);
        while let Some(id) = state.next_loan(&shares) {
            if !self.host.lend(&self.lent).unwrap_or(false) {
                break;
            }
            let run = state.runs.get_mut(&id).expect("BUG: a run just chosen");
            match run.lend(now) {
                None => {
                    self.give_host();
                    break;
                }
                Some(Some(wait)) => {
                    if state.recent.len() >= KEPT_RECENT {
                        state.recent.pop_front();
                    }
                    state.recent.push_back((now, wait));
                }
                Some(None) => {}
            }
        }
        self.reclaim(state);
        self.watch_saturation(state, now);
    }

    /// Starts, extends or ends the spell of saturation by the pools as
    /// counted at `now` (see the module docs).
    fn watch_saturation(&self, state: &mut State, now: Instant) {
        state.forget_waits(now);
        let active = state.active();
        let waiting = state.waiting();
        let saturated = active >= 2 && waiting >= 1 && self.host.free().unwrap_or(0) == 0;
        if saturated {
            state.calm_since = None;
            let spell = state.saturated.get_or_insert_with(|| Spell {
                since: now,
                since_unix: crate::clock::now(),
                lasted: Duration::ZERO,
                most_active: 0,
                most_waiting: 0,
            });
            spell.lasted = now.saturating_duration_since(spell.since);
            spell.most_active = spell.most_active.max(active);
            spell.most_waiting = spell.most_waiting.max(waiting);
            for run in state.runs.values_mut() {
                run.loans.saturated = true;
            }
        } else if state.saturated.is_some() {
            let calm_since = *state.calm_since.get_or_insert(now);
            // Fewer than two runs left is an end at once: the broker may
            // not look again before another comes.
            if active >= 2 && now.saturating_duration_since(calm_since) < CALM {
                return;
            }
            state.calm_since = None;
            let mut spell = state.saturated.take().expect("BUG: a spell just seen");
            spell.lasted = calm_since.saturating_duration_since(spell.since);
            if spell.lasted >= SATURATED_AFTER {
                (self.report)(&spell);
            }
        }
    }

    /// Decides which runs give slots back (see the module docs), and starts
    /// or stops their waiters.
    fn reclaim(self: &Arc<Self>, state: &mut State) {
        let wanted: Vec<(RunId, bool)> = state
            .runs
            .iter()
            .map(|(&id, run)| (id, self.wants_back(state, run)))
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

    /// Whether `run` gives slots back now (Reclaim, in the module docs):
    /// the host pool has no free slot, and another run waits that would use
    /// no more slots for its weight with one more than `run` would with one
    /// fewer, while `run` uses more than its share.
    fn wants_back(&self, state: &State, run: &Run) -> bool {
        if self.host.free().unwrap_or(0) != 0 {
            return false;
        }
        let Some(neediest) = state.neediest_waiting() else {
            return false;
        };
        let in_use = run.in_use();
        run.lent > run.free
            && yields(in_use.saturating_sub(1), run.weight, neediest)
            && f64::from(in_use) > self.shares(state).of(run)
    }

    /// Takes slots back from run `id` with `waiter` whenever the broker
    /// wants one, until the run ends. A slot it takes goes back to the host
    /// pool and is lent on at once, if a slot is still wanted back as it
    /// takes one; otherwise it goes back to the run pool. The waiter waits
    /// a while (in line with the run's processes) after the broker decided:
    /// the run that waited may no longer wait by then (its process between
    /// two slots, or a slot lent to it not yet taken), and the slot would
    /// be lent straight back to the run it was taken from.
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
            // Counted again as the slot is taken: the decision to take it
            // back, and where it goes, follow from this one count.
            let now = Instant::now();
            state.observe(now);
            let still_wanted = state
                .runs
                .get(&id)
                .is_some_and(|run| self.wants_back(&state, run));
            // Gone, the run took its pool with it, and the slot.
            let Some(run) = state.runs.get_mut(&id) else {
                return;
            };
            let Some(reclaim) = run.reclaim.as_mut() else {
                return;
            };
            reclaim.blocked = false;
            reclaim.wanted &= still_wanted;
            match taken {
                Ok(false) => {}
                Ok(true) if reclaim.wanted => {
                    run.lent = run.lent.saturating_sub(1);
                    run.loans.reclaimed += 1;
                    self.give_host();
                    self.settle(&mut state, now);
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
        state.shares(self.host.free().unwrap_or(0))
    }

    /// Gives a slot taken out of the host pool back to it. A pool already
    /// full drops it: it never grows beyond N.
    fn give_host(&self) {
        let _ = self.host.repay(&self.lent);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// The runs that use slots or wait for one.
    fn active(&self) -> u32 {
        let active = self.runs.values().filter(|run| run.active()).count();
        u32::try_from(active).unwrap_or(u32::MAX)
    }

    /// The runs that wait for a slot.
    fn waiting(&self) -> u32 {
        let waiting = self.runs.values().filter(|run| run.waiting()).count();
        u32::try_from(waiting).unwrap_or(u32::MAX)
    }

    /// Forgets the waits older than [`WINDOW`] at `now`.
    fn forget_waits(&mut self, now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|&(at, _)| now.saturating_duration_since(at) > WINDOW)
        {
            self.recent.pop_front();
        }
    }

    /// Counts every run pool's free slots.
    fn observe(&mut self, now: Instant) {
        for run in self.runs.values_mut() {
            run.observe(now);
        }
    }

    /// The capacity, with `free` slots in the host pool, and the active
    /// runs' weights.
    fn shares(&self, free: u32) -> Shares {
        let active = self.runs.values().filter(|run| run.active());
        Shares {
            capacity: free + active.clone().map(Run::holdings).sum::<u32>(),
            weights: active.map(|run| run.weight).sum(),
        }
    }

    /// The waiting runs, the neediest first: the fewest slots in use for
    /// their weight, then the heaviest, then the oldest.
    fn waiting_in_order(&self) -> Vec<(RunId, &Run)> {
        let mut waiting: Vec<(RunId, &Run)> = self
            .runs
            .iter()
            .filter(|(_, run)| run.waiting())
            .map(|(&id, run)| (id, run))
            .collect();
        waiting.sort_by(|(a_id, a), (b_id, b)| {
            a.relative_use()
                .total_cmp(&b.relative_use())
                .then(b.weight.total_cmp(&a.weight))
                .then(a_id.cmp(b_id))
        });
        waiting
    }

    /// The slots the neediest waiting run uses, and its weight.
    fn neediest_waiting(&self) -> Option<(u32, f64)> {
        self.waiting_in_order()
            .first()
            .map(|(_, run)| (run.in_use(), run.weight))
    }

    /// The run the next free slot of the host pool goes to: the neediest
    /// waiting run, unless it uses its share already while another run
    /// waits.
    fn next_loan(&self, shares: &Shares) -> Option<RunId> {
        let waiting = self.waiting_in_order();
        let alone = waiting.len() == 1;
        waiting
            .into_iter()
            .find(|(_, run)| alone || f64::from(run.in_use()) < shares.of(run))
            .map(|(id, _)| id)
    }
}

/// Whether a run of `weight` using `in_use` slots uses at least as many for
/// its weight as the `neediest` waiting run (its slots and weight) would
/// with one more: a slot given up then goes where it is needed more, and is
/// never lent straight back.
fn yields(in_use: u32, weight: f64, (neediest, its_weight): (u32, f64)) -> bool {
    f64::from(in_use) * its_weight >= f64::from(neediest + 1) * weight
}

impl Run {
    fn new(pool: Option<RunPool>, priority: Priority) -> Self {
        Self {
            weight: priority.weight(),
            pool,
            lent: 0,
            free: 0,
            surplus_since: None,
            last_free: Instant::now(),
            test_wanted: 0,
            test_held: 0,
            reclaim: None,
            unreclaimable: false,
            loans: Loans::default(),
        }
    }

    /// Counts the run pool's free slots.
    fn observe(&mut self, now: Instant) {
        let Some(pool) = &self.pool else {
            return;
        };
        self.free = pool.free().unwrap_or(0);
        if self.free > 0 {
            self.last_free = now;
        }
        if self.free > 1 {
            self.surplus_since.get_or_insert(now);
        } else {
            self.surplus_since = None;
        }
        self.loans.most = self.loans.most.max(self.in_use());
    }

    /// Whether the run pool's surplus (free slots beyond one) has lasted a
    /// [`TICK`] by `now`, and is no slot on its way to a waiting process.
    fn surplus_lasted(&self, now: Instant) -> bool {
        self.surplus_since
            .is_some_and(|since| now.saturating_duration_since(since) >= TICK)
    }

    /// Takes a free slot out of the run pool, if it has one.
    fn take_free(&mut self) -> bool {
        if self.free == 0 {
            return false;
        }
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

    /// Gives the run a slot taken from the host pool: a test slot when it
    /// waits for one, else a slot in its pool, with how long it waited for
    /// it. `None` when it could not, and the slot is still the caller's.
    fn lend(&mut self, now: Instant) -> Option<Option<Duration>> {
        if self.test_held < self.test_wanted {
            self.test_held += 1;
            return Some(None);
        }
        let pool = self.pool.as_ref()?;
        if !pool.give().unwrap_or(false) {
            return None;
        }
        self.lent += 1;
        let wait = now.saturating_duration_since(self.last_free);
        self.loans.record(wait);
        // A process waiting on a semaphore takes the slot as it is put in;
        // counted again at once, the run still waits if it did.
        self.observe(now);
        Some(Some(wait))
    }

    /// The slots the run uses: lent and not free in its pool, and its test
    /// slots.
    fn in_use(&self) -> u32 {
        self.lent.saturating_sub(self.free) + self.test_held
    }

    /// The slots the run uses for its weight.
    fn relative_use(&self) -> f64 {
        f64::from(self.in_use()) / self.weight
    }

    /// The slots taken from the host pool for the run.
    fn holdings(&self) -> u32 {
        self.lent + self.test_held
    }

    /// Whether the run may be waiting for a slot.
    fn waiting(&self) -> bool {
        self.test_held < self.test_wanted || (self.pool.is_some() && self.free == 0)
    }

    /// Whether the run uses or wants slots.
    fn active(&self) -> bool {
        self.in_use() > 0 || self.waiting()
    }
}

/// The capacity of the host pool for heli runs, and the weights of the runs
/// that share it.
struct Shares {
    capacity: u32,
    /// The sum of the active runs' weights.
    weights: f64,
}

impl Shares {
    /// The share of `run`: the capacity times its weight over the active
    /// runs' weights, `run` and what it holds among them.
    fn of(&self, run: &Run) -> f64 {
        let (capacity, weights) = if run.active() {
            (self.capacity, self.weights)
        } else {
            (self.capacity + run.holdings(), self.weights + run.weight)
        };
        f64::from(capacity) * run.weight / weights
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

#[cfg(test)]
mod tests {
    use super::{Run, State, yields};
    use crate::priority::Priority::{self, Background, Interactive, Normal};
    use crate::run_id::RunId;

    /// A run without a pool, using `in_use` slots (as test slots), and
    /// waiting for another when `waits`.
    fn run(priority: Priority, in_use: u32, waits: bool) -> Run {
        let mut run = Run::new(None, priority);
        run.test_held = in_use;
        run.test_wanted = in_use + u32::from(waits);
        run
    }

    fn state(runs: Vec<Run>) -> State {
        State {
            runs: runs
                .into_iter()
                .enumerate()
                .map(|(number, run)| (RunId::new(number as u64 + 1), run))
                .collect(),
            ..State::default()
        }
    }

    fn share(state: &State, free: u32, number: u64) -> f64 {
        state.shares(free).of(&state.runs[&RunId::new(number)])
    }

    #[test]
    fn shares_follow_the_weights() {
        // 4 : 1 of 10 slots.
        let both = state(vec![run(Interactive, 0, true), run(Normal, 0, true)]);
        assert_eq!(share(&both, 10, 1), 8.0);
        assert_eq!(share(&both, 10, 2), 2.0);
        // 1 : 1/4.
        let both = state(vec![run(Normal, 3, true), run(Background, 2, true)]);
        assert_eq!(share(&both, 5, 1), 8.0);
        assert_eq!(share(&both, 5, 2), 2.0);
        // 4 of 4.5 against two background runs.
        let three = state(vec![
            run(Background, 8, false),
            run(Background, 9, false),
            run(Interactive, 1, true),
        ]);
        assert_eq!(share(&three, 0, 3), 16.0);
        assert_eq!(share(&three, 0, 1), 1.0);
        // Equal weights: the capacity divided by the runs.
        let equal = state(vec![run(Background, 2, true), run(Background, 1, true)]);
        assert_eq!(share(&equal, 3, 1), 3.0);
    }

    #[test]
    fn a_new_run_counts_itself_in_with_its_weight() {
        let state = state(vec![run(Normal, 6, true), run(Interactive, 0, false)]);
        // Idle, the interactive run would share 6 slots with the other.
        assert!((share(&state, 0, 2) - 4.8).abs() < 1e-9);
        assert_eq!(share(&state, 0, 1), 6.0);
    }

    #[test]
    fn loans_go_to_the_fewest_in_use_for_their_weight() {
        // 1 for 1 against 3 for 4.
        let both = state(vec![run(Normal, 1, true), run(Interactive, 3, true)]);
        assert_eq!(both.next_loan(&both.shares(4)), Some(RunId::new(2)));
        // As many for their weights (none): the heavier first.
        let both = state(vec![run(Background, 0, true), run(Interactive, 0, true)]);
        assert_eq!(both.next_loan(&both.shares(4)), Some(RunId::new(2)));
        // And among equals, the oldest.
        let both = state(vec![run(Normal, 2, true), run(Normal, 2, true)]);
        assert_eq!(both.next_loan(&both.shares(4)), Some(RunId::new(1)));
    }

    #[test]
    fn a_run_at_its_share_waits_unless_alone() {
        // 17 slots: shares 1 and 16, each used.
        let both = state(vec![run(Background, 1, true), run(Interactive, 16, true)]);
        assert_eq!(both.next_loan(&both.shares(0)), None);
        // One under its share.
        let both = state(vec![run(Background, 1, true), run(Interactive, 15, true)]);
        assert_eq!(both.next_loan(&both.shares(1)), Some(RunId::new(2)));
        // A background run alone gets every slot, whatever the others hold.
        let alone = state(vec![run(Background, 7, true), run(Interactive, 9, false)]);
        assert_eq!(alone.next_loan(&alone.shares(1)), Some(RunId::new(1)));
    }

    #[test]
    fn slots_go_back_to_a_run_that_needs_them_more_for_its_weight() {
        // Equal weights: two slots more.
        assert!(yields(3 - 1, 1.0, (1, 1.0)));
        assert!(!yields(2 - 1, 1.0, (1, 1.0)));
        // A background run with 2 gives one to an interactive run with 6.
        assert!(yields(2 - 1, 0.25, (6, 4.0)));
        // An interactive run with 3 gives none to a background run with 0.
        assert!(!yields(3 - 1, 4.0, (0, 0.25)));
        // A normal run with 5 gives one to an interactive run with 15, not 16.
        assert!(yields(5 - 1, 1.0, (15, 4.0)));
        assert!(!yields(5 - 1, 1.0, (16, 4.0)));
    }
}
