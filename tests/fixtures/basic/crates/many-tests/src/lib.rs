//! 100 passing tests, `g0::t0` to `g9::t9`, each printing lines that the
//! test harness captures, and one ignored test, `ignored`: a test run
//! whose per-test lines make most of its full output.
//!
//! Each passing test prints its own path (`output of many_tests::g0::t0`),
//! then lines that look like what a runner prints of its own but are not:
//! two like nextest's status lines (`PASS [ 0.012s] many-tests g0::t0`), two
//! like libtest's own lines (`... not progress`, `running two tests`).

/// A passing test per name, printing its own path and the look-alikes;
/// failing instead when `HELI_FIXTURE_FAIL`, a comma-separated list of
/// paths (`g1::t2,g3::t4`), names it.
#[cfg(test)]
macro_rules! tests {
    ($($name:ident)*) => {
        $(
            #[test]
            fn $name() {
                let path = concat!(module_path!(), "::", stringify!($name));
                let failing = std::env::var("HELI_FIXTURE_FAIL").unwrap_or_default();
                if failing.split(',').any(|name| path.ends_with(&format!("::{name}"))) {
                    panic!("fixture failure: HELI_FIXTURE_FAIL names {path}");
                }
                println!(concat!("output of ", module_path!(), "::", stringify!($name)));
                println!("NOT [a] status line");
                println!("not a: status [   1.000s] line");
                println!("... not progress");
                println!("running two tests");
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

#[cfg(test)]
#[test]
#[ignore = "counted as ignored"]
fn ignored() {}
