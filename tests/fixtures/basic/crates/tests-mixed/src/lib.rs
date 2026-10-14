//! Tests that pass, fail and take a while (`HELI_FIXTURE_TEST_SLEEP_MS`,
//! default 300).

pub fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod tests {
    use super::double;

    #[test]
    fn passes() {
        assert_eq!(double(2), 4);
    }

    #[test]
    fn fails() {
        println!("captured output of the failing test");
        assert_eq!(double(2), 5, "fixture failure: expected to fail");
    }

    #[test]
    fn slow() {
        let ms = std::env::var("HELI_FIXTURE_TEST_SLEEP_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .unwrap_or(300);
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}
