//! The leaf of the workspace's chain of crates: it depends on `root`, and
//! nothing depends on it.

/// Tests change this line in a worktree to tell its build from another's.
const OWN: &str = "leaf from the commit";

/// Its own value, over `root`'s.
pub fn describe() -> String {
    format!("{OWN} over {}", root::VALUE)
}

#[cfg(test)]
mod tests {
    #[test]
    fn prints_what_it_describes() {
        println!("leaf: {}", super::describe());
    }
}
