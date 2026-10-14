//! Prints which source it was built from, then what the two packages
//! outside the workspace give it: `plain-dep`'s name, and the file
//! `outdir-dep`'s build script wrote, read at run time. Its test prints the
//! same line.

/// Tests change this line in a worktree to tell its build from another's.
const SOURCE: &str = "built from the commit";

/// What it prints.
fn line() -> String {
    format!(
        "{SOURCE}; {}; {}",
        plain_dep::NAME,
        outdir_dep::generated()
    )
}

fn main() {
    println!("{}", line());
}

#[cfg(test)]
mod tests {
    #[test]
    fn prints_its_line() {
        println!("app: {}", super::line());
    }
}
