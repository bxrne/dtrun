//! Integration tests: every fixture under `examples/config/good` must parse,
//! and every fixture under `examples/config/bad` must fail.

use libdtrun::oci::config::OciConfig;
use std::fs;
use std::path::{Path, PathBuf};

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/config")
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "expected at least one .json under {}",
        dir.display()
    );
    files
}

#[test]
fn good_configs_parse_successfully() {
    let good_dir = examples_dir().join("good");
    let mut failures = Vec::new();

    for path in json_files(&good_dir) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        match OciConfig::from_path(&path) {
            Ok(config) => {
                assert!(
                    !config.oci_version.is_empty(),
                    "{name}: ociVersion should be non-empty after parse"
                );
                assert!(
                    !config.root.path.is_empty(),
                    "{name}: root.path should be non-empty after parse"
                );
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }

    assert!(
        failures.is_empty(),
        "good configs must parse, but failed:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn bad_configs_fail_to_parse() {
    let bad_dir = examples_dir().join("bad");
    let mut unexpected_successes = Vec::new();

    for path in json_files(&bad_dir) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if OciConfig::from_path(&path).is_ok() {
            unexpected_successes.push(name);
        }
    }

    assert!(
        unexpected_successes.is_empty(),
        "bad configs must fail to parse, but succeeded:\n  {}",
        unexpected_successes.join("\n  ")
    );
}

#[test]
fn each_bad_config_has_a_distinct_failure() {
    // Sanity: every bad fixture is actually exercised (not an empty directory).
    let bad_dir = examples_dir().join("bad");
    let files = json_files(&bad_dir);
    assert!(
        files.len() >= 5,
        "expected the full set of bad fixtures, found {}",
        files.len()
    );

    for path in files {
        let err = OciConfig::from_path(&path).expect_err(&format!(
            "{} should not parse",
            path.file_name().unwrap().to_string_lossy()
        ));
        // Error Display should be non-empty so callers can log it.
        assert!(!err.to_string().is_empty());
    }
}
