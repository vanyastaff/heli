//! Uses `util`, so that unused-dependency checkers find nothing to report.

pub fn answer() -> u32 {
    util::base() + 1
}

#[cfg(feature = "extra")]
pub fn extra() -> u32 {
    answer() * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn answer_is_42() {
        assert_eq!(super::answer(), 42);
    }
}
