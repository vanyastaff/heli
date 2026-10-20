//! Saturation (docs/host-pool.md#saturation, #127): heli says when the pool
//! is saturated, in `status`, in the event log, and on the stderr of a run
//! of the whole workspace that joins it. Nothing is held back.
//!
//! The pool is saturated by two `jobs` runs (see `run_pools.rs`) with more
//! workers than the pool has slots: each worker takes a slot, holds it a
//! moment and asks for the next, so both runs always wait for one.

mod support;

use std::path::Path;
use std::process::{Child, Stdio};

use support::{Fixture, assert_success, runs};

/// A `jobs` run through heli, in the background, until its stop file
/// appears.
struct Jobs {
    child: Child,
    stop: std::path::PathBuf,
}

impl Jobs {
    fn start(fixture: &Fixture, jobs: &Path, name: &str, workers: u32) -> Self {
        let stop = fixture.path(&format!("{name}.stop"));
        let child = fixture
            .heli(["exec", "--"])
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

    fn stop(self) {
        std::fs::write(&self.stop, "").expect("write the stop file");
        let output = self.child.wait_with_output().expect("wait for heli");
        assert_success(&output, "cargo heli exec -- jobs");
    }
}

/// Two runs saturate a pool of 4: `status` counts the waiting runs, how
/// long it has lasted and the slot waits; a `check --workspace` that joins
/// is told to narrow itself, and checks; a `check -p` is not told; once the
/// runs end, the spell is in the event log.
#[test]
fn a_saturated_pool_is_reported_and_a_whole_workspace_run_is_told() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 4\n");
    let jobs = fixture.binary("jobs");
    let first = Jobs::start(&fixture, &jobs, "first", 6);
    fixture.wait_for_status("the first run", |status| runs(status).len() == 1);
    let second = Jobs::start(&fixture, &jobs, "second", 6);

    let status = fixture.wait_for_status("the pool to be saturated for 5 s", |status| {
        status["saturation"]["saturated_s"]
            .as_u64()
            .is_some_and(|lasted| lasted >= 6)
            && status["saturation"]["wait_p50_ms"].is_u64()
    });
    let pool = &status["saturation"];
    assert_eq!(pool["active_runs"], 2, "{status}");
    assert!(
        pool["waiting_runs"]
            .as_u64()
            .is_some_and(|waiting| waiting >= 1),
        "{status}"
    );
    assert_eq!(pool["window_s"], 600, "{status}");
    let text = fixture.heli(["status"]).output().expect("run status");
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(
        text.contains("4 slots: ") && text.contains("; saturated for "),
        "{text}"
    );
    assert!(text.contains("slot wait p50 "), "{text}");

    let narrow = fixture
        .heli(["check", "-p", "warnings"])
        .current_dir(fixture.workspace())
        .output()
        .expect("run heli");
    assert_success(&narrow, "cargo heli check -p warnings");
    let stderr = String::from_utf8_lossy(&narrow.stderr);
    assert!(!stderr.contains("Narrow it"), "{stderr}");

    let whole = fixture
        .heli([
            "check",
            "--workspace",
            "--exclude",
            "broken",
            "--exclude",
            "many-errors",
        ])
        .current_dir(fixture.workspace())
        .output()
        .expect("run heli");
    assert_success(&whole, "cargo heli check --workspace");
    let stderr = String::from_utf8_lossy(&whole.stderr);
    assert!(
        stderr.contains(" runs share 4 slots, saturated for ")
            && stderr.contains("Narrow it (--affected, -p) or run it with --priority background."),
        "{stderr}"
    );

    first.stop();
    second.stop();
    let spells = fixture.events_named("saturated");
    let spell = spells
        .first()
        .unwrap_or_else(|| panic!("no saturated event"));
    assert!(
        spell["seconds"].as_u64().is_some_and(|lasted| lasted >= 6),
        "{spell}"
    );
    assert_eq!(spell["slots"], 4, "{spell}");
    assert!(
        spell["most_active"].as_u64().is_some_and(|most| most >= 2),
        "{spell}"
    );
}
