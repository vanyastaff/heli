//! With `HELI_FIXTURE_MAKEFLAGS_LOG` set, writes the `CARGO_MAKEFLAGS` the
//! nested cargo gave this build script (its jobserver) to that file.

fn main() {
    println!("cargo:rerun-if-env-changed=HELI_FIXTURE_MAKEFLAGS_LOG");
    if let Some(log) = std::env::var_os("HELI_FIXTURE_MAKEFLAGS_LOG") {
        let makeflags = std::env::var("CARGO_MAKEFLAGS").unwrap_or_default();
        std::fs::write(log, makeflags).expect("write HELI_FIXTURE_MAKEFLAGS_LOG");
    }
}
