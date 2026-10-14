//! The memory governor and the bounded top-up (DESIGN.md §4): the daemon
//! withholds slots from the host pool while memory is low, returns them once
//! it has recovered, and gives the pool back the slots killed processes
//! lost, never more.
//!
//! Memory figures and the governor's clock come from the test seam
//! (`support::Seam`): a sample is taken when the clock has moved on by a
//! second.

mod support;

use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use support::{ConcurrencyLog, Fixture, Memory, assert_success, jobserver};

/// A megabyte, as the configuration and the seam count them.
const MB: u64 = 1 << 20;

/// The default watermarks of a 64 GB machine ([`Memory::available`]) are
/// 10 % of RAM, 6553.6 MB, and that plus 4 GB, 10649.6 MB: the megabytes
/// just below and above each.
const BELOW_LOW: u64 = 6553;
const ABOVE_LOW: u64 = 6554;
const BELOW_HIGH: u64 = 10_649;
const ABOVE_HIGH: u64 = 10_650;

/// Without injected figures, `status` shows the machine's memory: some
/// available, no more than its RAM, available commit on Windows only, and
/// the default watermarks 4 GB apart, the low one at least 6 GB.
#[test]
fn status_shows_the_machines_memory() {
    let fixture = Fixture::new("basic");
    fixture.use_real_memory();
    fixture.start_daemon();
    let status = fixture.wait_for_status("a sample", |status| !status["memory"].is_null());
    let memory = &status["memory"];
    let figure = |name: &str| {
        memory[name]
            .as_u64()
            .unwrap_or_else(|| panic!("{name}: {memory}"))
    };
    let (total, available) = (figure("total"), figure("available"));
    assert!(total >= 1 << 30, "{memory}");
    assert!(0 < available && available <= total, "{memory}");
    assert_eq!(
        memory["commit_available"].is_u64(),
        cfg!(windows),
        "{memory}"
    );
    let (low, high) = (figure("low_watermark"), figure("high_watermark"));
    assert!(low >= 6 << 30, "{memory}");
    assert_eq!(high - low, 4 << 30, "{memory}");
}

/// `cargo heli status` prints the memory on a line of its own.
#[test]
fn status_prints_the_memory_for_people() {
    let fixture = Fixture::new("basic");
    fixture.inject_memory(Memory {
        total_mb: 65_536,
        available_mb: 50_000,
        commit_available_mb: Some(40_000),
    });
    fixture.start_daemon();
    wait_for_sample(&fixture, 50_000);
    let output = fixture.heli(["status"]).output().expect("run heli status");
    assert_success(&output, "cargo heli status");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines().any(|line| line
            == "memory  48.8 GB of 64.0 GB available, commit 39.1 GB available; \
                slots withheld below 6.4 GB, returned above 10.4 GB"),
        "{text}"
    );
}

/// Moves the clock on by a second with `available_mb` injected (and as much
/// commit), and waits until `status` shows the sample: the governor has
/// acted on it. Successive calls must inject different figures, to tell the
/// samples apart.
fn sample(fixture: &Fixture, available_mb: u64) -> Value {
    sample_memory(fixture, Memory::available(available_mb))
}

/// [`sample`], with `memory` injected.
fn sample_memory(fixture: &Fixture, memory: Memory) -> Value {
    fixture.inject_memory(memory);
    fixture.advance_clock(1000);
    wait_for_sample(fixture, memory.available_mb)
}

/// A machine of 64 GB with `available_mb` available, and no commit figure
/// (as on Linux and macOS).
fn physical(available_mb: u64) -> Memory {
    Memory {
        total_mb: 65_536,
        available_mb,
        commit_available_mb: None,
    }
}

/// The status once the daemon shows a sample of `available_mb`.
fn wait_for_sample(fixture: &Fixture, available_mb: u64) -> Value {
    fixture.wait_for_status(&format!("a sample of {available_mb} MB"), |status| {
        status["memory"]["available"] == available_mb * MB
    })
}

/// A heli run in flight, waiting for its gate, until dropped: while it
/// runs, the pool is never quiet, so no top-up hides what the governor does.
struct InFlight {
    run: Option<Child>,
    gate: std::path::PathBuf,
}

impl InFlight {
    fn start(fixture: &Fixture) -> Self {
        let echo = fixture.echo_args();
        let gate = fixture.path("in-flight.gate");
        let run = fixture
            .heli(["exec", "--"])
            .arg(&echo)
            .env("ECHO_WAIT_FOR", &gate)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start heli");
        fixture.wait_for_status("the run", |status| !support::runs(status).is_empty());
        Self {
            run: Some(run),
            gate,
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.gate, "");
        if let Some(run) = self.run.take() {
            let _ = run.wait_with_output();
        }
    }
}

/// The pool `status --json` reports.
fn pool(slots: u32, free: u32, withheld: u32, in_use: u32) -> Value {
    json!({"slots": slots, "free": free, "withheld": withheld, "in_use": in_use})
}

/// Gives the daemon time for a few looks; nothing it does is awaited.
fn settle() {
    std::thread::sleep(Duration::from_millis(500));
}

/// Below the low watermark the daemon withholds one free slot per sample,
/// and always leaves the pool one slot. Without a sample (the clock stands
/// still), nothing more is withheld.
#[test]
fn low_memory_withholds_a_slot_per_sample_and_leaves_one() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    fixture.set_clock(0);
    fixture.inject_memory(Memory::available(2000));
    fixture.start_daemon();
    let first = wait_for_sample(&fixture, 2000);
    assert_eq!(first["pool"], pool(3, 2, 1, 0), "the first sample");
    settle();
    assert_eq!(fixture.status()["pool"], pool(3, 2, 1, 0), "no sample");

    assert_eq!(sample(&fixture, 1999)["pool"], pool(3, 1, 2, 0));
    for available in [1998, 1997] {
        assert_eq!(
            sample(&fixture, available)["pool"],
            pool(3, 1, 2, 0),
            "one slot is left"
        );
    }
    let withheld: Vec<(Value, Value)> = fixture
        .events_named("withhold")
        .iter()
        .map(|event| (event["withheld"].clone(), event["available"].clone()))
        .collect();
    assert_eq!(
        withheld,
        [(json!(1), json!(2000 * MB)), (json!(2), json!(1999 * MB))]
    );
}

/// Above the high watermark the withheld slots come back. Between the
/// watermarks nothing changes, whichever way memory goes: no slot is
/// withheld on the way up, and none returned on the way down.
#[test]
fn slots_come_back_above_the_high_watermark_without_oscillation() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    fixture.set_clock(0);
    fixture.inject_memory(physical(BELOW_LOW - 100));
    let _run = InFlight::start(&fixture);
    wait_for_sample(&fixture, BELOW_LOW - 100);
    let status = sample_memory(&fixture, physical(BELOW_LOW - 101));
    assert_eq!(status["pool"], pool(4, 2, 2, 0));

    for available in [ABOVE_LOW, BELOW_HIGH, BELOW_HIGH - 1] {
        assert_eq!(
            sample_memory(&fixture, physical(available))["pool"],
            pool(4, 2, 2, 0),
            "{available} MB, on the way up"
        );
    }
    let status = sample_memory(&fixture, physical(ABOVE_HIGH));
    assert_eq!(status["pool"], pool(4, 4, 0, 0));
    for available in [BELOW_HIGH, ABOVE_LOW, ABOVE_LOW + 1] {
        assert_eq!(
            sample_memory(&fixture, physical(available))["pool"],
            pool(4, 4, 0, 0),
            "{available} MB, on the way down"
        );
    }
    let status = sample_memory(&fixture, physical(BELOW_LOW));
    assert_eq!(status["pool"], pool(4, 3, 1, 0));

    let returned = fixture.events_named("return");
    assert_eq!(returned.len(), 1, "{returned:?}");
    assert_eq!(returned[0]["slots"], 2);
    assert_eq!(returned[0]["available"], ABOVE_HIGH * MB);
}

/// Polls `done` until it holds; panics after `seconds`, naming `what` it
/// waited for.
fn wait_until(what: &str, seconds: u64, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `cargo build --quiet` of the three `slow-*` crates, through heli or
/// (`bare`) as a bare cargo drawing from the fixture's pool, in a target
/// directory of its own, with its build scripts logging to `log` and
/// sleeping `sleep_ms`; started in the background.
fn start_slow_build(fixture: &Fixture, bare: bool, log: &ConcurrencyLog, sleep_ms: u64) -> Child {
    let args = [
        "build", "--quiet", "-p", "slow-a", "-p", "slow-b", "-p", "slow-c",
    ];
    let mut build = if bare {
        let mut cargo = fixture.cargo(args);
        cargo.env("CARGO_MAKEFLAGS", fixture.makeflags());
        // Its own process group, so that one signal kills it with its
        // build scripts ([`kill_mid_build`]).
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cargo, 0);
        cargo
    } else {
        fixture.heli(args)
    };
    build
        .env("CARGO_TARGET_DIR", fixture.path("target-slow"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    log.attach(&mut build, sleep_ms);
    build.spawn().expect("start the build")
}

/// Kills a bare build from [`start_slow_build`] at once, so that its cargo
/// cannot give back the slots it holds: on Windows cargo alone (its job
/// object then kills its build scripts), elsewhere its process group.
fn kill_mid_build(build: Child) {
    if cfg!(windows) {
        support::kill_process(build.id());
    } else {
        support::kill_tree(build.id());
    }
    let _ = build.wait_with_output();
}

/// Jobs that run as memory runs low are not interrupted: with both slots of
/// the pool held by build scripts, the governor withholds slots only as they
/// come free, and the build completes.
#[test]
fn a_build_in_progress_during_withholding_completes() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let log = fixture.concurrency_log();
    let build = start_slow_build(&fixture, false, &log, 3000);
    wait_until("three build scripts at once", 60, || log.running() == 3);
    assert_eq!(fixture.status()["pool"], pool(2, 0, 0, 2));

    fixture.inject_memory(Memory::available(1000));
    let output = build.wait_with_output().expect("wait for the build");
    assert_success(&output, "cargo heli build");
    let scripts = log.intervals();
    assert_eq!(scripts.len(), 3, "every script ran to its end: {scripts:?}");
    for rlib in ["slow_a", "slow_b", "slow_c"] {
        let path = fixture
            .path("target-slow")
            .join("debug")
            .join(format!("lib{rlib}.rlib"));
        assert!(path.is_file(), "{} was built", path.display());
    }
    let status =
        fixture.wait_for_status("a slot withheld", |status| status["pool"]["withheld"] == 1);
    assert_eq!(status["pool"], pool(2, 1, 1, 0));
}

/// Takes samples with `available_mb` injected and more each time, until
/// `done` holds of the status after one; panics after 20 samples. Returns
/// that status and the megabytes of the next sample.
fn sample_until(
    fixture: &Fixture,
    what: &str,
    mut available_mb: u64,
    done: impl Fn(&Value) -> bool,
) -> (Value, u64) {
    for _ in 0..20 {
        let status = sample(fixture, available_mb);
        available_mb += 1;
        if done(&status) {
            return (status, available_mb);
        }
    }
    panic!("no sample showed {what}: {}", fixture.status());
}

/// A slot that a process took and never gave back, because it died, comes
/// back once nothing holds the pool or compiles for 2 s: the top-up raises
/// the free slots to the target, N less the withheld slots, and never above
/// it.
#[test]
fn a_top_up_restores_lost_slots_up_to_the_target() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    fixture.set_clock(0);
    fixture.inject_memory(Memory::available(BELOW_LOW));
    fixture.start_daemon();
    assert_eq!(
        wait_for_sample(&fixture, BELOW_LOW)["pool"],
        pool(3, 2, 1, 0)
    );

    let client = jobserver::Client::open(&fixture.pool());
    client.take();
    drop(client);
    assert_eq!(fixture.status()["pool"], pool(3, 1, 1, 1), "a slot lost");
    let (status, next) = sample_until(&fixture, "the top-up", ABOVE_LOW, |status| {
        status["pool"]["in_use"] == 0
    });
    assert_eq!(status["pool"], pool(3, 2, 1, 0), "the target is 3 - 1");
    for available in next..next + 3 {
        assert_eq!(sample(&fixture, available)["pool"], pool(3, 2, 1, 0));
    }
    let top_ups = fixture.events_named("top-up");
    assert_eq!(top_ups.len(), 1, "{top_ups:?}");
    assert_eq!(
        (
            &top_ups[0]["slots"],
            &top_ups[0]["free"],
            &top_ups[0]["target"]
        ),
        (&json!(1), &json!(2), &json!(2))
    );
}

/// A bare cargo killed mid-build loses the slots it held; once nothing holds
/// the pool or compiles, `status` shows the full pool again.
#[test]
fn slots_a_killed_build_held_come_back_once_nothing_compiles() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    fixture.start_daemon();
    let log = fixture.concurrency_log();
    let build = start_slow_build(&fixture, true, &log, 60_000);
    wait_until("three build scripts at once", 60, || log.running() == 3);
    assert_eq!(fixture.status()["pool"], pool(2, 0, 0, 2));

    kill_mid_build(build);
    let status = fixture.wait_for_status("the full pool", |status| status["pool"]["in_use"] == 0);
    assert_eq!(status["pool"], pool(2, 2, 0, 0));
    let top_ups = fixture.events_named("top-up");
    assert_eq!(top_ups.len(), 1, "{top_ups:?}");
    assert_eq!(
        (
            &top_ups[0]["slots"],
            &top_ups[0]["free"],
            &top_ups[0]["target"]
        ),
        (&json!(2), &json!(2), &json!(2))
    );
}

/// A daemon that adopts a pool from which processes took slots and died
/// gives the slots back once nothing else holds the pool.
#[test]
fn slots_lost_before_a_restart_come_back_to_the_adopted_pool() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    fixture.set_clock(0);
    fixture.start_daemon();
    // Keeps the pool alive while no daemon holds it.
    let holder = jobserver::Client::open(&fixture.pool());
    for _ in 0..2 {
        let client = jobserver::Client::open(&fixture.pool());
        client.take();
        drop(client);
    }
    let daemon = fixture.daemon_pid().expect("a daemon");
    support::kill_process(daemon);
    fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());

    fixture.start_daemon();
    assert_eq!(
        fixture.status()["pool"],
        pool(2, 0, 0, 2),
        "adopted as it is"
    );
    drop(holder);
    let (status, _) = sample_until(&fixture, "the top-up", 49_000, |status| {
        status["pool"]["in_use"] == 0
    });
    assert_eq!(status["pool"], pool(2, 2, 0, 0));
    let log = fixture.daemon_log();
    let openings: Vec<&str> = log
        .lines()
        .filter_map(|line| line.split_once("] pool "))
        .map(|(_, rest)| rest)
        .collect();
    assert_eq!(openings.len(), 2, "{log}");
    assert!(openings[1].contains(": adopted, "), "{log}");
}

/// A top-up waits until no heli run has been in flight for 2 s: a run's
/// processes may hold slots they will give back.
#[test]
fn a_top_up_waits_2_s_after_the_last_run() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    fixture.set_clock(0);
    let echo = fixture.echo_args();
    fixture.start_daemon();
    let gate = fixture.path("gate");
    let run = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .env("ECHO_WAIT_FOR", &gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli");
    fixture.wait_for_status("the run", |status| !support::runs(status).is_empty());
    let lost = jobserver::Client::open(&fixture.pool());
    lost.take();
    drop(lost);
    for available in 40_000..40_004 {
        assert_eq!(
            sample(&fixture, available)["pool"],
            pool(2, 1, 0, 1),
            "while the run is in flight"
        );
    }

    std::fs::write(&gate, "").expect("open the gate");
    assert_success(&run.wait_with_output().expect("wait"), "cargo heli exec");
    fixture.wait_for_status("the run to end", |status| support::runs(status).is_empty());
    for (available, waited) in [(40_010, 0), (40_011, 1000)] {
        assert_eq!(
            sample(&fixture, available)["pool"],
            pool(2, 1, 0, 1),
            "quiet for {waited} ms"
        );
    }
    assert_eq!(
        sample(&fixture, 40_012)["pool"],
        pool(2, 2, 0, 0),
        "quiet for 2 s"
    );
}

/// Free slots above the target (here, a slot given back that was never
/// taken, while one is withheld) are taken out of the pool at once: with
/// the clock standing still, and a process holding the pool.
#[test]
fn surplus_slots_are_drained_back_to_the_target() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    fixture.set_clock(0);
    fixture.inject_memory(Memory::available(BELOW_LOW));
    fixture.start_daemon();
    assert_eq!(
        wait_for_sample(&fixture, BELOW_LOW)["pool"],
        pool(3, 2, 1, 0)
    );

    let client = jobserver::Client::open(&fixture.pool());
    client.give(1);
    wait_until("the drain", 30, || {
        !fixture.events_named("drain").is_empty()
    });
    assert_eq!(fixture.status()["pool"], pool(3, 2, 1, 0));
    let drains = fixture.events_named("drain");
    assert_eq!(drains.len(), 1, "{drains:?}");
    assert_eq!(
        (
            &drains[0]["slots"],
            &drains[0]["free"],
            &drains[0]["target"]
        ),
        (&json!(1), &json!(2), &json!(2))
    );
}

/// On Unix, bytes written into the FIFO beyond its N slots (a FIFO has no
/// maximum count) are drained back to N.
#[cfg(unix)]
#[test]
fn surplus_bytes_in_the_fifo_are_drained() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    fixture.set_clock(0);
    fixture.start_daemon();
    let client = jobserver::Client::open(&fixture.pool());
    client.give(3);
    wait_until("the drain", 30, || {
        !fixture.events_named("drain").is_empty()
    });
    assert_eq!(fixture.status()["pool"], pool(2, 2, 0, 0));
    let drains = fixture.events_named("drain");
    assert_eq!(drains.len(), 1, "{drains:?}");
    assert_eq!(drains[0]["slots"], 3, "{drains:?}");
}

/// Runs `echo-args` with `arg` through heli.
fn run_echo(fixture: &Fixture, echo: &std::path::Path, arg: &str) {
    let output = fixture
        .heli(["exec", "--"])
        .arg(echo)
        .arg(arg)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
}

/// The event log records each admission: the run, its worktree and its
/// command, with the time.
#[test]
fn the_event_log_records_admissions() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    run_echo(&fixture, &echo, "first");
    run_echo(&fixture, &echo, "second");
    let admitted = fixture.events_named("admit");
    assert_eq!(admitted.len(), 2, "{admitted:?}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs();
    for (event, (run, arg)) in admitted.iter().zip([("r1", "first"), ("r2", "second")]) {
        assert_eq!(event["run"], run, "{event}");
        assert_eq!(
            event["argv"],
            json!([echo.to_str().expect("a UTF-8 path"), arg]),
            "{event}"
        );
        let worktree = event["worktree"].as_str().expect("a worktree");
        assert_eq!(
            std::fs::canonicalize(worktree).expect("the worktree exists"),
            std::fs::canonicalize(fixture.workspace()).expect("the workspace exists"),
        );
        let at = event["at"].as_u64().expect("seconds since 1970");
        assert!((now - 120..=now).contains(&at), "{event}");
    }
}

/// Past its size limit the event log rotates: it becomes `<name>.1`,
/// replacing the one before, and the next event starts a new one. So at
/// most two files of about the limit remain, with the latest events.
#[test]
fn the_event_log_rotates() {
    let fixture = Fixture::new("basic");
    fixture.configure("[event_log]\nmax_kb = 1\n");
    let echo = fixture.echo_args();
    for run in 1..=15 {
        run_echo(&fixture, &echo, &format!("run {run}"));
    }
    let files: Vec<(String, u64)> = std::fs::read_dir(fixture.state_dir())
        .expect("the state directory")
        .map(|entry| entry.expect("an entry"))
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("events"))
        .map(|entry| {
            let size = entry.metadata().expect("its size").len();
            (entry.file_name().to_string_lossy().into_owned(), size)
        })
        .collect();
    assert!(files.len() <= 2, "{files:?}");
    assert!(
        files.iter().any(|(name, _)| name.ends_with(".log.1")),
        "{files:?}"
    );
    for (name, size) in &files {
        assert!(*size <= 1024 + 512, "{name}: {size} bytes");
    }
    let runs: Vec<Value> = fixture
        .events_named("admit")
        .iter()
        .map(|event| event["run"].clone())
        .collect();
    assert_eq!(runs.last(), Some(&json!("r15")), "{runs:?}");
    assert!(
        !runs.contains(&json!("r1")),
        "the oldest are gone: {runs:?}"
    );
}

/// The configuration sets the watermarks: the low one alone (the high one
/// is then 4 GB above it), both, or the high one alone.
#[test]
fn the_watermarks_can_be_configured() {
    for (settings, below_low, between, above_high) in [
        ("low_watermark_mb = 20000", 19_999, 24_095, 24_097),
        (
            "low_watermark_mb = 20000\nhigh_watermark_mb = 22000",
            19_999,
            21_999,
            22_001,
        ),
        ("high_watermark_mb = 30000", BELOW_LOW, 29_999, 30_001),
    ] {
        let fixture = Fixture::new("basic");
        fixture.configure(&format!("[pool]\nslots = 2\n[memory]\n{settings}\n"));
        fixture.set_clock(0);
        fixture.inject_memory(Memory::available(below_low));
        fixture.start_daemon();
        let first = wait_for_sample(&fixture, below_low);
        assert_eq!(first["pool"], pool(2, 1, 1, 0), "{settings}");
        assert_eq!(
            sample(&fixture, between)["pool"],
            pool(2, 1, 1, 0),
            "{settings}"
        );
        assert_eq!(
            sample(&fixture, above_high)["pool"],
            pool(2, 2, 0, 0),
            "{settings}"
        );
    }
}

/// Watermarks the daemon cannot use keep it from starting: the command runs
/// directly after one warning, and the daemon's log says why.
#[test]
fn unusable_watermarks_are_refused() {
    for settings in [
        "low_watermark_mb = 9000\nhigh_watermark_mb = 9000",
        "high_watermark_mb = 6000",
        "low_watermark_mb = \"lots\"",
    ] {
        let fixture = Fixture::new("basic");
        fixture.configure(&format!("[memory]\n{settings}\n"));
        let echo = fixture.echo_args();
        let output = fixture
            .heli(["exec", "--"])
            .arg(&echo)
            .output()
            .expect("run heli");
        assert_success(&output, "cargo heli exec");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with("cargo-heli: warning: the heli daemon exited with code 1"),
            "{settings}: {stderr}"
        );
        let log = fixture.daemon_log();
        assert!(log.contains("watermark"), "{settings}: {log}");
    }
}

/// Available commit (a figure Windows has) counts too: below the low
/// watermark slots are withheld however much physical memory is available,
/// and they come back only once both are above the high one.
#[test]
fn low_commit_withholds_slots_too() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    fixture.set_clock(0);
    let memory = |available_mb, commit_mb| Memory {
        total_mb: 65_536,
        available_mb,
        commit_available_mb: Some(commit_mb),
    };
    fixture.inject_memory(memory(40_000, 1000));
    let _run = InFlight::start(&fixture);
    assert_eq!(wait_for_sample(&fixture, 40_000)["pool"], pool(3, 2, 1, 0));
    let status = sample_memory(&fixture, memory(40_001, BELOW_HIGH));
    assert_eq!(status["pool"], pool(3, 2, 1, 0));
    let status = sample_memory(&fixture, memory(40_002, ABOVE_HIGH));
    assert_eq!(status["pool"], pool(3, 3, 0, 0));
}

/// `status` shows the memory the governor saw last and its watermarks: by
/// default, slots are withheld below the larger of 10 % of RAM and 6 GB, and
/// returned above that plus 4 GB.
#[test]
fn status_shows_memory_and_the_watermarks() {
    for (total_mb, low_mb) in [(100_000, 10_000), (40_000, 6144)] {
        let fixture = Fixture::new("basic");
        fixture.inject_memory(Memory {
            total_mb,
            available_mb: 30_000,
            commit_available_mb: Some(20_000),
        });
        fixture.start_daemon();
        assert_eq!(
            fixture.status()["memory"],
            json!({
                "total": total_mb * MB,
                "available": 30_000 * MB,
                "commit_available": 20_000 * MB,
                "low_watermark": low_mb * MB,
                "high_watermark": (low_mb + 4096) * MB,
            }),
            "{total_mb} MB of RAM"
        );
    }
}
