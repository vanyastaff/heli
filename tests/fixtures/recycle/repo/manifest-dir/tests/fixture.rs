//! Reads `fixtures/data.txt` through the package directory compiled into
//! the test, and prints it: `fixture: <its first line>`.

#[test]
fn reads_its_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("data.txt");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    println!("fixture: {}", text.lines().next().unwrap_or_default());
}
