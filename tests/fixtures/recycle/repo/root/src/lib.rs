//! The root of the workspace's chain of crates: `leaf` depends on it.

/// Tests change this line in a worktree to tell its build from another's.
pub const VALUE: &str = "root from the commit";

/// A text file compiled in, byte for byte.
pub const NOTE: &str = include_str!("../note.txt");

#[cfg(test)]
mod tests {
    #[test]
    fn prints_its_value() {
        println!("root: {}; note {:?}", super::VALUE, super::NOTE);
    }
}
