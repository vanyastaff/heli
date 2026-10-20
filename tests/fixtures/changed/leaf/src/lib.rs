pub fn leaf() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn one() {
        assert_eq!(super::leaf(), 1);
    }

    #[test]
    fn two() {
        assert_eq!(super::leaf() * 2, 2);
    }
}
