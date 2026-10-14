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

/// Builds the `jobs` fixture and returns its path.
fn jobs_binary(fixture: &Fixture) -> PathBuf {
    let output = fixture
        .cargo(["build", "--quiet", "-p", "jobs"])
        .output()
        .expect("run cargo");
    assert_success(&output, "building jobs");
    fixture
        .target_dir()
        .join("debug")
        .join(format!("jobs{}", std::env::consts::EXE_SUFFIX))
}

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
        let stop = fixture.path(&format!("{name}.stop"));
        let idle = fixture.path(&format!("{name}.idle"));
        let pid = fixture.path(&format!("{name}.pid"));
        let child = fixture
            .heli(["exec", "--"])
            .arg(jobs)
            .env("JOBS_WORKERS", workers.to_string())
            .env("JOBS_HOLD_MS", hold_ms.to_string())
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
    let jobs = jobs_binary(&fixture);
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
/// daemon logs it when the run ends: a run alone that borrowed all 4 slots
/// had at least 4 loans, each within a fraction of a second.
#[test]
fn the_wait_for_a_lent_slot_is_logged() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = jobs_binary(&fixture);
    let alone = Jobs::start(&fixture, &jobs, "alone", 6, 50);
    let status = fixture.wait_for_status("the run to use every slot", |status| {
        runs(status).len() == 1 && runs(status)[0]["slots"] == 4
    });
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    alone.stop();
    let log = daemon_log(&fixture);
    let line = run_pool_line(&log, &id);
    let loans: u64 = line
        .split_once(" loans")
        .and_then(|(loans, _)| loans.parse().ok())
        .unwrap_or_else(|| panic!("no loans: {line}"));
    assert!(loans >= 4, "{line}");
    let millis = |label: &str| -> f64 {
        line.split_once(label)
            .and_then(|(_, rest)| rest.split_once(" ms"))
            .and_then(|(value, _)| value.parse().ok())
            .unwrap_or_else(|| panic!("no {label:?} in {line}"))
    };
    let (median, longest) = (
        millis("wait for a lent slot: median "),
        millis(", longest "),
    );
    assert!(median <= longest && longest < 1000.0, "{line}");
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
    let jobs = jobs_binary(fixture);
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
    let log = daemon_log(&fixture);
    assert!(taken_back(&log, &a) <= 2, "{log}");
    assert_eq!(taken_back(&log, &b), 0, "{log}");
}

/// How many slots run `id` gave back while it had work pending, as the
/// daemon's log line for its run pool says.
fn taken_back(log: &str, id: &str) -> u64 {
    run_pool_line(log, id)
        .split(", ")
        .find_map(|part| part.strip_suffix(" taken back"))
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no count of slots taken back from {id}: {log}"))
}

/// The daemon's own log, whatever its session suffix.
fn daemon_log(fixture: &Fixture) -> String {
    std::fs::read_dir(fixture.state_dir())
        .expect("the state directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("daemon") && name.ends_with(".log")
        })
        .map(|path| std::fs::read_to_string(path).expect("read the daemon's log"))
        .collect()
}

/// The daemon's log line on run `id`'s run pool, from after `run pool: `.
fn run_pool_line<'a>(log: &'a str, id: &str) -> &'a str {
    let marker = format!("] {id}: run pool: ");
    log.lines()
        .find_map(|line| line.split_once(&marker).map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("no run pool line for {id}: {log}"))
}

/// A run whose processes stop asking for slots keeps one free in its pool,
/// for the next one that asks, and gives the others back: with N = 4, all
/// but one are free again while the run goes on.
#[test]
fn an_idle_run_keeps_one_free_slot() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = jobs_binary(&fixture);
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
    let jobs = jobs_binary(&fixture);
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

/// A run killed while its processes hold every slot gives them all back at
/// once: the first `status` without the run shows every slot free, without
/// waiting for the pool to be quiet.
#[test]
fn a_killed_run_returns_all_its_loans_at_once() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    let jobs = jobs_binary(&fixture);
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
