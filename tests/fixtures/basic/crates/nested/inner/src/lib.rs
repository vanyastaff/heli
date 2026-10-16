pub const INNER: u32 = 1;

#[cfg(test)]
mod tests {
    #[test]
    fn reads_build_output_after_the_runtime_gate() {
        let Some(ready) = std::env::var_os("HELI_FIXTURE_RUNTIME_READY") else {
            return;
        };
        let asset = std::path::Path::new(env!("OUT_DIR")).join("fixture-artifact");
        assert!(
            asset.exists(),
            "build output exists at the start of test runtime"
        );
        std::fs::write(ready, asset.to_string_lossy().as_bytes()).expect("publish runtime asset");
        let gate = std::path::PathBuf::from(
            std::env::var_os("HELI_FIXTURE_RUNTIME_GATE").expect("runtime gate"),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !gate.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "runtime gate timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::metadata(&asset)
                .expect("OUT_DIR asset survives GC during test runtime")
                .len(),
            64 << 20,
        );
        if let Some(done) = std::env::var_os("HELI_FIXTURE_RUNTIME_DONE") {
            std::fs::write(done, "asset read successfully").expect("publish runtime completion");
        }
    }
}
