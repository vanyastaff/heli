//! Compiles with warnings from several lints: unused_imports,
//! unused_variables, unused_mut and dead_code.

use std::collections::HashMap;

pub fn compute() -> u32 {
    let unused = 1;
    let mut never_mutated = 2;
    never_mutated
}

fn never_called() {}
