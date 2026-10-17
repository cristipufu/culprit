//! Every integration test file in `tests/`, as a module of this one test binary: one link (and one scan of a fresh
//! binary) for all of them, where a binary per file relinks every one after each change. A new file in `tests/` is
//! listed here, or `every_test_file_is_listed` fails.

mod escalations;
mod file_pickers;
mod golden;
mod insights;
mod plans;
mod plugin;
mod share;
mod table_growth;

#[test]
fn every_test_file_is_listed() {
    let listed = include_str!("all.rs");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(&dir).expect("tests/") {
        let path = entry.expect("an entry").path();
        let Some(name) =
            path.file_stem().and_then(|n| n.to_str()).filter(|_| path.extension().is_some_and(|e| e == "rs"))
        else {
            continue;
        };
        if name != "all" {
            assert!(
                listed.contains(&format!("\nmod {name};")),
                "tests/{name}.rs isn't listed in tests/all.rs: add `mod {name};`"
            );
        }
    }
}
