//! Four tests that show how many run at once. Each appends
//! `<test> start <ns>` and `<test> end <ns>` lines to `HELI_FIXTURE_LOG`
//! (when set), and in between waits until the file `HELI_FIXTURE_GATE`
//! exists (when set; 60 s at most).

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn stamp(test: &str, event: &str) {
        let Some(log) = std::env::var_os("HELI_FIXTURE_LOG") else {
            return;
        };
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos();
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .and_then(|mut file| file.write_all(format!("{test} {event} {nanos}\n").as_bytes()))
            .expect("append to HELI_FIXTURE_LOG");
    }

    fn gated(test: &str) {
        stamp(test, "start");
        if let Some(gate) = std::env::var_os("HELI_FIXTURE_GATE") {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !Path::new(&gate).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        stamp(test, "end");
    }

    #[test]
    fn a() {
        gated("a");
    }

    #[test]
    fn b() {
        gated("b");
    }

    #[test]
    fn c() {
        gated("c");
    }

    #[test]
    fn d() {
        gated("d");
    }
}
