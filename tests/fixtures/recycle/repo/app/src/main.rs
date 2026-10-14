//! Prints which source it was built from, then what the two packages
//! outside the workspace give it: `plain-dep`'s name, and the file
//! `outdir-dep`'s build script wrote, read at run time.

/// Tests change this line in a worktree to tell its build from another's.
const SOURCE: &str = "built from the commit";

fn main() {
    println!(
        "{SOURCE}; {}; {}",
        plain_dep::NAME,
        outdir_dep::generated()
    );
}
