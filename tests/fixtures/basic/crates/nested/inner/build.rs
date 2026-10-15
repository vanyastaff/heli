//! With `HELI_FIXTURE_MAKEFLAGS_LOG` set, writes the `CARGO_MAKEFLAGS` the
//! nested cargo gave this build script (its jobserver) to that file.

fn main() {
    println!("cargo:rerun-if-env-changed=HELI_FIXTURE_ARTIFACT_MB");
    if let Ok(megabytes) = std::env::var("HELI_FIXTURE_ARTIFACT_MB") {
        let megabytes: u64 = megabytes.parse().expect("artifact megabytes");
        let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
            .join("fixture-artifact");
        std::fs::File::create(path)
            .expect("create fixture artifact")
            .set_len(megabytes * (1 << 20))
            .expect("size fixture artifact");
    }
    println!("cargo:rerun-if-env-changed=HELI_FIXTURE_INNER_GATE");
    if let Some(gate) = std::env::var_os("HELI_FIXTURE_INNER_GATE") {
        let gate = std::path::PathBuf::from(gate);
        while !gate.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    println!("cargo:rerun-if-env-changed=HELI_FIXTURE_MAKEFLAGS_LOG");
    if let Some(log) = std::env::var_os("HELI_FIXTURE_MAKEFLAGS_LOG") {
        let makeflags = std::env::var("CARGO_MAKEFLAGS").unwrap_or_default();
        std::fs::write(log, makeflags).expect("write HELI_FIXTURE_MAKEFLAGS_LOG");
    }
}
