//! Shared test helpers for Mycel integration and black-box tests.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use mycel_core::Db;
use serde::de::DeserializeOwned;

/// Open an in-memory `Db` with all migrations applied.
pub fn test_db() -> Db {
    Db::open_in_memory().expect("open in-memory test db")
}

/// Return the absolute path to a fixture file relative to the crate root.
///
/// Usage: `fixture_path("tests/fixtures/decay_cases.jsonl")`
pub fn fixture_path(rel: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR is set by the test runner to the crate root.
    let base = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR must be set in test context");
    Path::new(&base).join(rel)
}

/// Read the contents of a fixture file as a string.
pub fn read_fixture(rel: &str) -> String {
    std::fs::read_to_string(fixture_path(rel))
        .unwrap_or_else(|e| panic!("failed to read fixture {rel}: {e}"))
}

/// Load a JSONL file into a `Vec<T>`.
///
/// Skips blank lines. Panics with a descriptive message on parse errors.
pub fn load_jsonl<T: DeserializeOwned>(rel: &str) -> Vec<T> {
    let path = fixture_path(rel);
    let file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("failed to open fixture {}: {e}", path.display()));
    let reader = BufReader::new(file);
    let mut items = Vec::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.unwrap_or_else(|e| panic!("io error at line {line_no}: {e}"));
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let item: T = serde_json::from_str(trimmed).unwrap_or_else(|e| {
            panic!(
                "failed to parse line {} of {}: {e}\n  content: {trimmed}",
                line_no + 1,
                path.display()
            )
        });
        items.push(item);
    }
    items
}
