//! An integration test of `leaf`, which a test deletes in a worktree.

#[test]
fn prints_what_leaf_describes() {
    println!("extra: {}", leaf::describe());
}
