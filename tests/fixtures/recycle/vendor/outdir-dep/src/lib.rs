//! A package outside the workspace that compiles in a path under its build
//! script's `OUT_DIR`, and reads the file there at run time.

/// What the build script wrote, or why it cannot be read.
pub fn generated() -> String {
    let path = concat!(env!("OUT_DIR"), "/generated.txt");
    std::fs::read_to_string(path).unwrap_or_else(|error| format!("cannot read {path}: {error}"))
}
