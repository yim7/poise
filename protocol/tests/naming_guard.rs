use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_PATTERNS: &[&str] = &["Grid", "grid_", "grid_id", "target_exposure", "target exposure"];

#[test]
fn protocol_source_uses_track_and_desired_exposure_names() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    collect_violations(&src_dir, &mut violations);
    assert!(
        violations.is_empty(),
        "protocol crate still contains legacy naming:\n{}",
        violations.join("\n")
    );
}

fn collect_violations(dir: &Path, violations: &mut Vec<String>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", dir.display());
    });

    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read directory entry: {err}"));
        let path = entry.path();
        if path.is_dir() {
            collect_violations(&path, violations);
            continue;
        }

        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }

        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        collect_file_violations(path, &contents, violations);
    }
}

fn collect_file_violations(path: PathBuf, contents: &str, violations: &mut Vec<String>) {
    for pattern in FORBIDDEN_PATTERNS {
        if contents.contains(pattern) {
            violations.push(format!("{} contains `{pattern}`", path.display()));
        }
    }
}
