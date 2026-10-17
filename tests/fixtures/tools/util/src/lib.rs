pub fn base() -> u32 {
    41
}

#[cfg(test)]
mod tests {
    #[test]
    fn base_is_41() {
        assert_eq!(super::base(), 41);
    }
}
