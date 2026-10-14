//! The process scan of the bounded top-up (DESIGN.md §4): while another
//! process holds the host pool open, a top-up also waits for every process
//! that runs a cargo job to end.
//!
//! The scan sees every such process of the user (on Windows, of the logon
//! session), not only this test's: compilers that other tests or programs
//! run keep the top-up waiting too. So this test has a binary of its own,
//! which `cargo test` runs alone; where other compilers run meanwhile, it
//! still passes, but proves less.

mod support;

use std::io::BufRead;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use support::{Fixture, Memory, jobserver};

/// The pool `status --json` reports.
fn pool(slots: u32, free: u32, withheld: u32, in_use: u32) -> Value {
    json!({"slots": slots, "free": free, "withheld": withheld, "in_use": in_use})
}

/// Moves the governor's clock on by a second with `available_mb` injected,
/// and returns the status once the daemon shows that sample.
fn sample(fixture: &Fixture, available_mb: u64) -> Value {
    fixture.inject_memory(Memory::available(available_mb));
    fixture.advance_clock(1000);
    fixture.wait_for_status("a sample", |status| {
        status["memory"]["available"] == available_mb << 20
    })
}

/// A slot lost by a process that died stays lost while another process
/// holds the pool and a `rustc`, `rustdoc`, `clippy-driver` or build script
/// runs: each may run a job whose slot will come back. Once nothing else
/// holds the pool, it comes back.
#[test]
fn a_top_up_waits_while_anything_compiles() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    fixture.set_clock(0);
    let echo = fixture.echo_args();
    fixture.start_daemon();
    let holder = jobserver::Client::open(&fixture.pool());
    let lost = jobserver::Client::open(&fixture.pool());
    lost.take();
    drop(lost);
    assert_eq!(fixture.status()["pool"], pool(2, 1, 0, 1));

    let mut available = 40_000;
    for name in ["rustc", "rustdoc", "clippy-driver", "build-script-build"] {
        let program = fixture.path(&format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(&echo, &program).expect("copy echo-args");
        let gate = fixture.path(&format!("{name}.gate"));
        let mut compiler = Command::new(&program)
            .arg("started")
            .env("ECHO_WAIT_FOR", &gate)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start a compiler");
        let mut started = String::new();
        std::io::BufReader::new(compiler.stdout.as_mut().expect("its stdout"))
            .read_line(&mut started)
            .expect("read from the compiler");
        assert_eq!(started, "arg: \"started\"\n", "{name} runs");
        for _ in 0..4 {
            assert_eq!(
                sample(&fixture, available)["pool"],
                pool(2, 1, 0, 1),
                "while {name} runs"
            );
            available += 1;
        }
        std::fs::write(&gate, "").expect("open the gate");
        compiler.wait().expect("wait for the compiler");
    }

    drop(holder);
    for _ in 0..20 {
        if sample(&fixture, available)["pool"] == pool(2, 2, 0, 0) {
            return;
        }
        available += 1;
    }
    panic!("the lost slot never came back: {}", fixture.status());
}
