use std::path::PathBuf;

/// An example Cargo built for these tests. Cargo has moved test binaries
/// between releases, so the examples folder is found among the test binary's
/// ancestors rather than at a fixed depth.
pub fn example(name: &str) -> PathBuf {
    let test = std::env::current_exe().unwrap();
    test.ancestors()
        .map(|folder| folder.join("examples").join(name))
        .find(|example| example.is_file())
        .unwrap_or_else(|| panic!("no example named {name} above {}", test.display()))
}
