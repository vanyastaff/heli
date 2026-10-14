//! The bounded top-up under stress (DESIGN.md §4, #26): while bare cargos
//! draw from the host pool and are killed at random, losing the slots they
//! hold, no top-up ever lets more than N jobs hold slots at once, and no
//! cargo is refused a slot it gives back. Each time they are gone and the
//! pool is quiet, the lost slots come back.
//!
//! The cargos are the `jobs` fixture under the name `cargo`: jobserver
//! clients that take slots for their jobs and give them back, logging when
//! they hold one, with no compiler of their own. A cargo takes a slot before
//! it starts the job it is for, so the process scan must count a live cargo
//! as a holder: else the top-up, finding nothing compiling, would give the
//! slots it holds to the pool again.
//!
//! This test has a binary of its own: the process scan sees every process
//! of the user (on Windows, of the logon session). Its runtime is bounded:
//! four rounds of 2 s of load, each followed by the wait for the top-up.

mod support;

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::Fixture;

/// N, the pool's slots.
const SLOTS: usize = 3;

/// The rounds of load: in each, cargos are started and killed at random for
/// [`ROUND`], then every one left is killed, and the lost slots must come
/// back.
const ROUNDS: usize = 4;

/// How long the cargos of a round are started and killed.
const ROUND: Duration = Duration::from_secs(2);

/// Now, in nanoseconds since 1970, as the `jobs` log has it.
fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos()
}

/// A pseudo-random number generator (xorshift), seeded from the clock: the
/// order of kills changes from run to run, and the seed is printed.
struct Random(u64);

impl Random {
    fn new() -> Self {
        let seed = (now_ns() as u64) | 1;
        eprintln!("seed {seed}");
        Self(seed)
    }

    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// A cargo drawing from the pool: its process, and when it was killed.
struct Cargo {
    child: Child,
    killed_at: Option<u128>,
}

/// The most slots the logged holds had at the same moment. A hold that
/// never ended belongs to a cargo that was killed holding it: it ended as
/// the cargo did.
fn most_held(log: &str, killed: &BTreeMap<u32, u128>) -> usize {
    let mut open: BTreeMap<&str, u128> = BTreeMap::new();
    let mut events: Vec<(u128, i32)> = Vec::new();
    for line in log.lines() {
        let mut fields = line.split_whitespace();
        let (Some(holder), Some(event), Some(at)) = (fields.next(), fields.next(), fields.next())
        else {
            // A cargo killed as it wrote.
            continue;
        };
        let Ok(at) = at.parse::<u128>() else {
            continue;
        };
        match event {
            "start" => {
                open.insert(holder, at);
            }
            "end" => {
                if let Some(start) = open.remove(holder) {
                    events.extend([(start, 1), (at, -1)]);
                }
            }
            other => panic!("unknown event {other:?} in {line:?}"),
        }
    }
    for (holder, start) in open {
        let pid: u32 = holder
            .split('.')
            .next()
            .and_then(|pid| pid.parse().ok())
            .expect("a process id");
        let end = killed.get(&pid).copied().unwrap_or(u128::MAX);
        events.extend([(start, 1), (end, -1)]);
    }
    // At equal times an end sorts before a start.
    events.sort();
    let (mut now, mut most) = (0i32, 0i32);
    for (_, delta) in events {
        now += delta;
        most = most.max(now);
    }
    usize::try_from(most).expect("never negative")
}

#[test]
fn no_top_up_lets_more_than_n_jobs_hold_slots_while_cargos_are_killed() {
    let fixture = Fixture::new("basic");
    fixture.configure(&format!("[pool]\nslots = {SLOTS}\n"));
    fixture.set_clock(0);
    fixture.start_daemon();
    let bin = fixture.path("bin");
    std::fs::create_dir_all(&bin).expect("create a bin directory");
    let cargo = bin.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
    support::copy_program(&fixture.binary("jobs"), &cargo);
    let log = fixture.path("held.log");
    let start = |random: &mut Random, index: usize| {
        let stderr = std::fs::File::create(fixture.path(&format!("cargo-{index}.stderr")))
            .expect("create a stderr file");
        // Some take slots and give them back quickly, some keep each for
        // longer than a top-up waits for quiet: a slot taken for a job not
        // yet started.
        let hold = if random.below(3) == 0 { 2_500 } else { 30 };
        Command::new(&cargo)
            .env("CARGO_MAKEFLAGS", fixture.makeflags())
            .env("JOBS_WORKERS", "2")
            .env("JOBS_HOLD_MS", hold.to_string())
            .env("JOBS_LOG", &log)
            .env("JOBS_MAX_MS", "60000")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("start a cargo")
    };

    // The governor's clock runs fast, a sample every 150 ms: a top-up
    // could happen at any moment of the load.
    let ticking = AtomicBool::new(true);
    let (cargos, started) = std::thread::scope(|scope| {
        let ticker = scope.spawn(|| {
            while ticking.load(Ordering::Relaxed) {
                fixture.advance_clock(1000);
                std::thread::sleep(Duration::from_millis(150));
            }
        });
        let mut random = Random::new();
        let mut cargos: Vec<Cargo> = Vec::new();
        let mut started = 0;
        for round in 0..ROUNDS {
            let loading = Instant::now();
            while loading.elapsed() < ROUND {
                while cargos
                    .iter()
                    .filter(|cargo| cargo.killed_at.is_none())
                    .count()
                    < 3
                {
                    cargos.push(Cargo {
                        child: start(&mut random, started),
                        killed_at: None,
                    });
                    started += 1;
                }
                std::thread::sleep(Duration::from_millis(100 + random.below(300) as u64));
                let alive: Vec<usize> = (0..cargos.len())
                    .filter(|&index| cargos[index].killed_at.is_none())
                    .collect();
                kill(&mut cargos[alive[random.below(alive.len())]]);
            }
            cargos.iter_mut().for_each(kill);
            // Nothing holds the pool now: once it has been quiet for 2 s,
            // the slots the killed cargos took with them come back.
            let waiting = Instant::now();
            while fixture.status()["pool"]["free"] != SLOTS {
                assert!(
                    waiting.elapsed() < Duration::from_secs(30),
                    "round {round}: the lost slots never came back: {}",
                    fixture.status()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        ticking.store(false, Ordering::Relaxed);
        ticker.join().expect("the clock's thread");
        (cargos, started)
    });
    let killed: BTreeMap<u32, u128> = cargos
        .iter()
        .filter_map(|cargo| Some((cargo.child.id(), cargo.killed_at?)))
        .collect();

    let held = std::fs::read_to_string(&log).unwrap_or_default();
    // Each round's first cargos have the pool's slots to themselves; how
    // many more they get depends on the machine.
    assert!(
        held.lines().count() >= 2 * SLOTS * ROUNDS,
        "the cargos drew from the pool: {} lines",
        held.lines().count()
    );
    let most = most_held(&held, &killed);
    assert!(most <= SLOTS, "{most} slots held at once, of {SLOTS}");
    for index in 0..started {
        let stderr = std::fs::read_to_string(fixture.path(&format!("cargo-{index}.stderr")))
            .unwrap_or_default();
        assert!(
            !stderr.contains("cannot give a slot back"),
            "a cargo was refused a slot it held: {stderr}"
        );
    }
}

/// Kills `cargo`, unless it was killed already, noting when.
fn kill(cargo: &mut Cargo) {
    if cargo.killed_at.is_none() {
        let _ = cargo.child.kill();
        let _ = cargo.child.wait();
        cargo.killed_at = Some(now_ns());
    }
}
