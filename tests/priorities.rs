//! Priorities (DESIGN.md §4, "Priorities"): a run's weight in the broker's
//! shares, interactive 4, normal 1, background ¼.
//!
//! The runs here are the `jobs` fixture (see `run_pools.rs`): each worker
//! takes a slot, holds it a moment, gives it back and asks for the next, so
//! a run with more workers than its share always has work pending. Tests
//! have no terminal: every run is normal unless `--priority` or
//! `HELI_PRIORITY` says otherwise.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use support::{Fixture, assert_success, runs};

/// How a run is given its priority.
#[derive(Clone, Copy)]
enum Given {
    Flag(&'static str),
    Var(&'static str),
    Default,
}

/// A `jobs` run through heli, in the background.
struct Jobs {
    child: Child,
    stop: PathBuf,
}

impl Jobs {
    /// Starts `cargo heli [--priority <p>] exec -- jobs` with `workers`
    /// workers, each holding a slot for 50 ms at a time; `name` tells its
    /// files apart.
    fn start(fixture: &Fixture, jobs: &Path, name: &str, workers: u32, given: Given) -> Self {
        let stop = fixture.path(&format!("{name}.stop"));
        let mut command = match given {
            Given::Flag(priority) => fixture.heli(["--priority", priority, "exec", "--"]),
            Given::Var(_) | Given::Default => fixture.heli(["exec", "--"]),
        };
        if let Given::Var(priority) = given {
            command.env("HELI_PRIORITY", priority);
        }
        let child = command
            .arg(jobs)
            .env("JOBS_WORKERS", workers.to_string())
            .env("JOBS_HOLD_MS", "50")
            .env("JOBS_GAP_MS", "2")
            .env("JOBS_STOP", &stop)
            .env("JOBS_IDLE", fixture.path(&format!("{name}.idle")))
            .env("JOBS_PID", fixture.path(&format!("{name}.pid")))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start heli");
        Self { child, stop }
    }

    /// Tells the run to end, and waits for its client.
    fn stop(self) {
        std::fs::write(&self.stop, "").expect("write the stop file");
        let output = self.child.wait_with_output().expect("wait for heli");
        assert_success(&output, "cargo heli exec -- jobs");
    }
}

/// The run `id` in a `status --json` document.
fn run<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["runs"]
        .as_array()
        .and_then(|runs| runs.iter().find(|run| run["id"] == id))
        .unwrap_or_else(|| panic!("no run {id}: {status}"))
}

fn slots(status: &Value, id: &str) -> u64 {
    run(status, id)["slots"].as_u64().unwrap_or(0)
}

/// The ids of the runs in `status` not among `known`.
fn new_runs(status: &Value, known: &[String]) -> Vec<String> {
    runs(status)
        .iter()
        .map(|run| run["id"].as_str().expect("an id").to_owned())
        .filter(|id| !known.contains(id))
        .collect()
}

/// Starts the `first` runs and waits until they use every one of `pool`
/// slots, then starts `last` and waits for ten `status` samples in a row
/// (half a second) in which every run uses its weighted share, within one
/// slot, and they use every slot. Returns the runs, their ids in the order
/// given, and the first of those samples.
fn settle(
    fixture: &Fixture,
    pool: u64,
    first: &[(u32, Given)],
    last: (u32, Given),
    shares: &[f64],
) -> (Vec<Jobs>, Vec<String>, Value) {
    let jobs = fixture.binary("jobs");
    let mut started = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for (index, &(workers, given)) in first.iter().enumerate() {
        started.push(Jobs::start(
            fixture,
            &jobs,
            &format!("run{index}"),
            workers,
            given,
        ));
        let status =
            fixture.wait_for_status("the run", |status| runs(status).len() == ids.len() + 1);
        ids.extend(new_runs(&status, &ids));
    }
    fixture.wait_for_status("the first runs to use every slot", |status| {
        ids.iter().map(|id| slots(status, id)).sum::<u64>() == pool
    });
    let (workers, given) = last;
    started.push(Jobs::start(fixture, &jobs, "last", workers, given));
    let status =
        fixture.wait_for_status("the last run", |status| runs(status).len() == ids.len() + 1);
    let arrived = Instant::now();
    ids.extend(new_runs(&status, &ids));
    let last_id = ids.last().expect("the last run").clone();
    fixture.wait_for_status("the last run to get a slot", |status| {
        slots(status, &last_id) > 0
    });
    let first_slot = arrived.elapsed();
    // Its first slot comes as soon as a worker of another run gives one
    // back (50 ms), give or take the start of `jobs`, which on a loaded
    // host can take seconds.
    assert!(
        first_slot < Duration::from_secs(10),
        "the last run waited {first_slot:?} for a slot"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut settled = Vec::new();
    let mut samples = Vec::new();
    while settled.len() < 10 {
        let status = fixture.status();
        let used: Vec<u64> = ids.iter().map(|id| slots(&status, id)).collect();
        let at_share = used
            .iter()
            .zip(shares)
            .all(|(&used, &share)| (used as f64 - share).abs() <= 1.0);
        if at_share && used.iter().sum::<u64>() == pool {
            settled.push(status);
        } else {
            settled.clear();
        }
        samples.push(used);
        assert!(
            Instant::now() < deadline,
            "the runs never settled at {shares:?}: {samples:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    (started, ids, settled.swap_remove(0))
}

fn assert_shares(status: &Value, ids: &[String], shares: &[f64]) {
    for (id, &share) in ids.iter().zip(shares) {
        let got = run(status, id)["share"].as_f64().expect("a share");
        assert!(
            (got - share).abs() < 1e-9,
            "{id}: share {got}, not {share}: {status}"
        );
    }
}

/// With the pool saturated by two background runs (N = 18), an interactive
/// run gets a slot at once and settles at its weighted share, 4 of 4.5:
/// 16 slots, the background runs keeping 1 each. `status` shows each run's
/// priority.
#[test]
fn an_interactive_run_takes_its_share_from_background_runs() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 18\n");
    let shares = [1.0, 1.0, 16.0];
    let (started, ids, settled) = settle(
        &fixture,
        18,
        &[
            (10, Given::Flag("background")),
            (10, Given::Var("background")),
        ],
        (18, Given::Flag("interactive")),
        &shares,
    );
    assert_shares(&settled, &ids, &shares);
    for (id, priority) in ids.iter().zip(["background", "background", "interactive"]) {
        assert_eq!(run(&settled, id)["priority"], priority, "{settled}");
    }
    let text = fixture.heli(["status"]).output().expect("run heli status");
    let text = String::from_utf8_lossy(&text.stdout);
    let line = |id: &str| {
        text.lines()
            .find(|line| line.contains(&format!("  {id}  ")))
            .unwrap_or_else(|| panic!("no {id} in {text}"))
            .to_owned()
    };
    assert!(line(&ids[2]).contains("  interactive  "), "{text}");
    assert!(line(&ids[0]).contains("  background  "), "{text}");
    for jobs in started {
        jobs.stop();
    }
    assert_eq!(fixture.status()["pool"]["free"], 18);
}

/// Interactive against normal, 4 : 1 of N = 10: 8 slots and 2.
#[test]
fn interactive_and_normal_share_four_to_one() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 10\n");
    let shares = [2.0, 8.0];
    let (started, ids, settled) = settle(
        &fixture,
        10,
        &[(12, Given::Default)],
        (12, Given::Var("interactive")),
        &shares,
    );
    assert_shares(&settled, &ids, &shares);
    assert_eq!(run(&settled, &ids[0])["priority"], "normal", "{settled}");
    for jobs in started {
        jobs.stop();
    }
}

/// Normal against background, 1 : ¼ of N = 10: 8 slots and 2, whichever
/// came first.
#[test]
fn normal_and_background_share_four_to_one() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 10\n");
    let shares = [2.0, 8.0];
    let (started, ids, settled) = settle(
        &fixture,
        10,
        &[(12, Given::Flag("background"))],
        (12, Given::Flag("normal")),
        &shares,
    );
    assert_shares(&settled, &ids, &shares);
    for jobs in started {
        jobs.stop();
    }
}

/// `HELI_PRIORITY` with a value that is no priority is a usage error.
#[test]
fn an_unknown_priority_is_refused() {
    let fixture = Fixture::new("basic");
    let output = fixture
        .heli(["exec", "--", "true"])
        .env("HELI_PRIORITY", "urgent")
        .output()
        .expect("run heli");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("HELI_PRIORITY=urgent"), "{stderr}");
}
