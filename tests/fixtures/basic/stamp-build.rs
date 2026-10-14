//! Build script shared by the `slow-*` crates. With `HELI_FIXTURE_LOG` set it
//! appends `<crate> start <ns>` and `<crate> end <ns>` lines to that file
//! around a sleep of `HELI_FIXTURE_SLEEP_MS`, so a test can tell how many
//! build jobs ran at once.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn stamp(log: &Path, event: &str) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let line = format!("{} {event} {nanos}\n", env!("CARGO_PKG_NAME"));
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .expect("append to HELI_FIXTURE_LOG");
}

fn main() {
    println!("cargo:rerun-if-env-changed=HELI_FIXTURE_LOG");
    println!("cargo:rerun-if-env-changed=HELI_FIXTURE_SLEEP_MS");
    let Some(log) = std::env::var_os("HELI_FIXTURE_LOG") else {
        return;
    };
    let log = Path::new(&log);
    stamp(log, "start");
    let sleep = std::env::var("HELI_FIXTURE_SLEEP_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(sleep));
    stamp(log, "end");
}
