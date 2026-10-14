//! Run pools and the broker (DESIGN.md §4, "Run pools"): each run draws from
//! a pool of its own, which the daemon's broker fills from the host pool.
//!
//! The runs here are the `jobs` fixture: a jobserver client whose workers
//! each take a slot, hold it a moment, give it back and ask for the next,
//! so a run with more workers than slots always has work pending.

mod support;

use std::path::PathBuf;
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use support::{Fixture, assert_success, runs};

/// A `jobs` run through heli, in the background.
struct Jobs {
    child: Child,
    stop: PathBuf,
    idle: PathBuf,
    pid: PathBuf,
}

impl Jobs {
    /// Starts `cargo heli exec -- jobs` with `workers` workers, each holding
    /// a slot for `hold_ms` at a time; `name` tells its files apart.
    fn start(fixture: &Fixture, jobs: &PathBuf, name: &str, workers: u32, hold_ms: u64) -> Self {
        Self::start_with_gap(fixture, jobs, name, workers, hold_ms, 2)
    }

    /// [`Jobs::start`], with each worker waiting `gap_ms` between giving a
    /// slot back and asking for the next.
    fn start_with_gap(
        fixture: &Fixture,
        jobs: &PathBuf,
        name: &str,
        workers: u32,
        hold_ms: u64,
        gap_ms: u64,
    ) -> Self {
        let stop = fixture.path(&format!("{name}.stop"));
        let idle = fixture.path(&format!("{name}.idle"));
        let pid = fixture.path(&format!("{name}.pid"));
        let child = fixture
            .heli(["exec", "--"])
            .arg(jobs)
            .env("JOBS_WORKERS", workers.to_string())
            .env("JOBS_HOLD_MS", hold_ms.to_string())
            .env("JOBS_GAP_MS", gap_ms.to_string())
            .env("JOBS_STOP", &stop)
            .env("JOBS_IDLE", &idle)
            .env("JOBS_PID", &pid)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start heli");
        Self {
            child,
            stop,
            idle,
            pid,
        }
    }

    /// Tells the run's workers to give their slots back and ask for no
    /// more; the run goes on.
    fn idle(&self) {
        std::fs::write(&self.idle, "").expect("write the idle file");
    }

    /// Tells the run to end, and waits for its client.
    fn stop(self) -> Output {
        std::fs::write(&self.stop, "").expect("write the stop file");
        let output = self.child.wait_with_output().expect("wait for heli");
        assert_success(&output, "cargo heli exec -- jobs");
        output
    }
}

/// The run `id` in a `status --json` document.
fn run<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["runs"]
        .as_array()
        .and_then(|runs| runs.iter().find(|run| run["id"] == id))
        .unwrap_or_else(|| panic!("no run {id}: {status}"))
}

/// A run alone borrows every slot of the host pool: with N = 4 and six
/// workers wanting slots, `status` shows it using all 4, with a share of
/// 4, and none free. Once it ends, all 4 are free again.
#[test]
fn a_run_alone_borrows_every_slot() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = fixture.binary("jobs");
    let alone = Jobs::start(&fixture, &jobs, "alone", 6, 50);
    let status = fixture.wait_for_status("the run to use every slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 4
    });
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    assert_eq!(run(&status, &id)["share"].as_f64(), Some(4.0), "{status}");
    assert_eq!(status["pool"]["free"], 0, "{status}");
    alone.stop();
    let status = fixture.status();
    assert_eq!(status["pool"]["free"], 4, "{status}");
}

/// The broker measures how long a run waits for each slot it lends, and the
/// daemon writes it to the event log when the run ends: a run alone that
/// borrowed all 4 slots had at least 4 loans, each within a fraction of a
/// second, and used at most 4.
#[test]
fn the_wait_for_a_lent_slot_is_logged() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = fixture.binary("jobs");
    let alone = Jobs::start(&fixture, &jobs, "alone", 6, 50);
    let status = fixture.wait_for_status("the run to use every slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 4
    });
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    alone.stop();
    let loans = loans(&fixture, &id);
    assert!(loans["loans"].as_u64() >= Some(4), "{loans}");
    assert_eq!(loans["most"], 4, "{loans}");
    let (median, longest) = (
        loans["wait_median_ms"].as_f64().expect("a median wait"),
        loans["wait_longest_ms"].as_f64().expect("a longest wait"),
    );
    assert!(median <= longest && longest < 1000.0, "{loans}");
}

/// The `loans` event of run `id`: how its loans went, logged as it ended.
fn loans(fixture: &Fixture, id: &str) -> Value {
    fixture
        .events_named("loans")
        .into_iter()
        .find(|event| event["run"] == id)
        .unwrap_or_else(|| panic!("no loans event for {id}: {:?}", fixture.events()))
}

/// The slots run `id` uses, as `status --json` shows them; 0 when it is not
/// listed.
fn slots(status: &Value, id: &str) -> u64 {
    status["runs"]
        .as_array()
        .and_then(|runs| runs.iter().find(|run| run["id"] == id))
        .and_then(|run| run["slots"].as_u64())
        .unwrap_or(0)
}

/// Two `jobs` runs with eight workers each in a pool of `slots`: the second
/// starts once the first holds every slot. Returns both, their ids, and the
/// first of ten `status` samples in a row (half a second) in which they use
/// every slot, within one of each other.
fn balance_two_busy_runs(fixture: &Fixture, slots_n: u64) -> (Jobs, Jobs, String, String, Value) {
    let jobs = fixture.binary("jobs");
    let first = Jobs::start(fixture, &jobs, "first", 8, 50);
    let status = fixture.wait_for_status("the first run to use every slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == slots_n
    });
    let a = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    let second = Jobs::start(fixture, &jobs, "second", 8, 50);
    let status = fixture.wait_for_status("the second run", |status| runs(status).len() == 2);
    let b = runs(&status)
        .iter()
        .map(|run| run["id"].as_str().expect("an id").to_owned())
        .find(|id| *id != a)
        .expect("the second run");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut balanced = Vec::new();
    let mut samples = Vec::new();
    while balanced.len() < 10 {
        let status = fixture.status();
        let (used_a, used_b) = (slots(&status, &a), slots(&status, &b));
        samples.push((used_a, used_b));
        if used_a.abs_diff(used_b) <= 1 && used_a + used_b == slots_n {
            balanced.push(status);
        } else {
            balanced.clear();
        }
        assert!(
            Instant::now() < deadline,
            "the runs never balanced: {samples:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let first_balanced = balanced.swap_remove(0);
    (first, second, a, b, first_balanced)
}

/// Two runs with work pending share a saturated pool. With N = 4, the first
/// gives slots back as its workers finish with them, until the two use
/// within one slot of each other, and stay so, each with a share of 2.
#[test]
fn two_busy_runs_converge_to_within_one_slot() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let (first, second, a, b, balanced) = balance_two_busy_runs(&fixture, 4);
    for id in [&a, &b] {
        assert_eq!(
            run(&balanced, id)["share"].as_f64(),
            Some(2.0),
            "{balanced}"
        );
    }
    first.stop();
    second.stop();
    assert_eq!(fixture.status()["pool"]["free"], 4);
}

/// Balanced runs keep their slots: with N = 5, the first gives back no more
/// than the two slots that make it 3 to 2, and the second never gives one
/// back, though both still want more.
#[test]
fn balanced_runs_keep_their_slots() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 5\n");
    let (first, second, a, b, _) = balance_two_busy_runs(&fixture, 5);
    std::thread::sleep(Duration::from_millis(500));
    first.stop();
    second.stop();
    let (first, second) = (loans(&fixture, &a), loans(&fixture, &b));
    assert!(first["taken_back"].as_u64() <= Some(2), "{first}");
    assert_eq!(second["taken_back"], 0, "{second}");
}

/// A slot is taken back from a run only for another run that waits at
/// that moment, and goes to it. Taken while the poorer run had just
/// stopped waiting (its process between two slots, or a slot lent to it
/// not yet taken), it would be lent straight back to the run it came from:
/// a take-back that moves nothing, and counts as one. With N = 4: a run
/// whose three workers keep three slots busy, and a run whose one worker
/// takes a slot for a moment now and then. Every slot taken back from the
/// first was lent to the second.
#[test]
fn a_slot_taken_back_goes_to_the_run_that_waits() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = fixture.binary("jobs");
    let busy = Jobs::start(&fixture, &jobs, "busy", 3, 50);
    let status = fixture.wait_for_status("the first run to use three slots", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 3
    });
    let a = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    let now_and_then = Jobs::start_with_gap(&fixture, &jobs, "now-and-then", 1, 5, 100);
    let status = fixture.wait_for_status("the second run", |status| runs(status).len() == 2);
    let b = runs(&status)
        .iter()
        .map(|run| run["id"].as_str().expect("an id").to_owned())
        .find(|id| *id != a)
        .expect("the second run");
    std::thread::sleep(Duration::from_secs(2));
    busy.stop();
    now_and_then.stop();
    let (busy, now_and_then) = (loans(&fixture, &a), loans(&fixture, &b));
    assert!(
        busy["taken_back"].as_u64() <= now_and_then["loans"].as_u64(),
        "taken back from the first: {busy}; lent to the second: {now_and_then}"
    );
}

/// A run whose processes stop asking for slots keeps one free in its pool,
/// for the next one that asks, and gives the others back: with N = 4, all
/// but one are free again while the run goes on.
#[test]
fn an_idle_run_keeps_one_free_slot() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = fixture.binary("jobs");
    let quiet = Jobs::start(&fixture, &jobs, "quiet", 6, 50);
    fixture.wait_for_status("the run to use every slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 4
    });
    quiet.idle();
    let status = fixture.wait_for_status("the run to use no slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 0
    });
    let status = if status["pool"]["free"] == 3 {
        status
    } else {
        fixture.wait_for_status("three free slots", |status| status["pool"]["free"] == 3)
    };
    assert_eq!(runs(&status).len(), 1, "the run goes on: {status}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.status()["pool"]["free"], 3, "and keeps one");
    quiet.stop();
}

/// A run that keeps a free slot it does not use gives it to a run that
/// waits with fewer slots in use: with N = 2, a run holding one slot (and
/// keeping the other free) and a second run asking for two end with one
/// each.
#[test]
fn a_spare_slot_goes_to_a_run_that_waits() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let jobs = fixture.binary("jobs");
    let holder = Jobs::start(&fixture, &jobs, "holder", 1, 600_000);
    let status = fixture.wait_for_status("the first run to hold a slot and keep one", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 1 && status["pool"]["free"] == 0
    });
    let a = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    let asker = Jobs::start(&fixture, &jobs, "asker", 2, 50);
    let status = fixture.wait_for_status("the second run to get the spare slot", |status| {
        runs(status).len() == 2 && runs(status).iter().all(|run| run["slots"] == 1)
    });
    assert_eq!(slots(&status, &a), 1, "{status}");
    holder.stop();
    asker.stop();
}

/// The host pool's drain counts the broker's loans: with N = 4 and a run
/// holding 2 slots and keeping a third free, a slot given to the host pool
/// beyond its target (N less the 3 lent) is a surplus, and is drained.
#[test]
fn a_surplus_beside_loans_is_drained() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = fixture.binary("jobs");
    let holding = Jobs::start(&fixture, &jobs, "holding", 2, 600_000);
    fixture.wait_for_status("2 slots held and 1 kept", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 2 && status["pool"]["free"] == 1
    });
    support::jobserver::Client::open(&fixture.pool()).give(1);
    let status = fixture.wait_for_status("the surplus drained", |status| {
        !fixture.events_named("drain").is_empty() && status["pool"]["free"] == 1
    });
    assert_eq!(runs(&status)[0]["slots"], 2, "{status}");
    let drains = fixture.events_named("drain");
    assert_eq!(drains[0]["target"], 1, "{drains:?}");
    holding.stop();
}

/// A run killed while its processes hold every slot gives them all back at
/// once: the first `status` without the run shows every slot free, without
/// waiting for the pool to be quiet.
#[test]
fn a_killed_run_returns_all_its_loans_at_once() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    let jobs = fixture.binary("jobs");
    let held = Jobs::start(&fixture, &jobs, "held", 3, 600_000);
    fixture.wait_for_status("the run to hold every slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 3
    });
    let pid: u32 = std::fs::read_to_string(&held.pid)
        .expect("the run's pid")
        .parse()
        .expect("a pid");
    support::kill_process(pid);
    let output = held.child.wait_with_output().expect("wait for heli");
    assert!(!output.status.success(), "a killed run fails");
    let status = fixture.wait_for_status("the run to end", |status| runs(status).is_empty());
    assert_eq!(status["pool"]["free"], 3, "{status}");
}
