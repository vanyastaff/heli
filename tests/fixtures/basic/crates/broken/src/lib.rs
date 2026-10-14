//! Does not compile: a type error (E0308) and an unresolved name (E0425).

pub fn number() -> u32 {
    "not a number"
}

pub fn missing() -> u32 {
    not_defined_anywhere
}
