//! With `HELI_FIXTURE_GATE` set, the build script waits until that file
//! exists (60 s at most), so a test decides when a build of this crate ends.

use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    println!("cargo:rerun-if-env-changed=HELI_FIXTURE_GATE");
    let Some(gate) = std::env::var_os("HELI_FIXTURE_GATE") else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !Path::new(&gate).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}
