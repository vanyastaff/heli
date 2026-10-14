//! 100 passing tests, `g0::t0` to `g9::t9`, each printing a line that the
//! test harness captures: a test run whose per-test lines make most of its
//! full output.

/// A passing test per name, printing its own path.
#[cfg(test)]
macro_rules! tests {
    ($($name:ident)*) => {
        $(
            #[test]
            fn $name() {
                println!(concat!("output of ", module_path!(), "::", stringify!($name)));
            }
        )*
    };
}

/// A module of ten tests per name.
#[cfg(test)]
macro_rules! groups {
    ($($group:ident)*) => {
        $(
            mod $group {
                tests!(t0 t1 t2 t3 t4 t5 t6 t7 t8 t9);
            }
        )*
    };
}

#[cfg(test)]
groups!(g0 g1 g2 g3 g4 g5 g6 g7 g8 g9);
