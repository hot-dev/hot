//! Secret-like `HOT_*` settings must be read through
//! `hot::environment::read_optional` or the CLI's configuration overlay. Both
//! accept `<NAME>_FILE`, which is how self-hosted installs deliver secrets; a
//! direct `std::env::var` read sees only a plaintext environment value.

use std::path::{Path, PathBuf};

/// Name fragments that mark a setting as a secret or a credential-bearing URI.
const SECRET_MARKERS: &[&str] = &["SECRET", "PASSWORD", "TOKEN", "_KEY", "_URI"];
const DIRECT_READS: &[&str] = &["env::var(\"", "env::var_os(\""];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            // Integration tests set up their own fixtures.
            if !matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("tests" | "target")
            ) {
                rust_sources(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn direct_secret_reads(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for read in DIRECT_READS {
        for (at, _) in source.match_indices(read) {
            let name: String = source[at + read.len()..]
                .chars()
                .take_while(|c| *c != '"')
                .collect();
            if name.starts_with("HOT_")
                && !name.starts_with("HOT_TEST_")
                && SECRET_MARKERS.iter().any(|marker| name.contains(marker))
            {
                found.push(name);
            }
        }
    }
    found
}

#[test]
fn secret_settings_are_never_read_directly_from_the_environment() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut sources = Vec::new();
    rust_sources(crates, &mut sources);
    assert!(sources.len() > 100, "expected to scan the workspace crates");
    let offenders: Vec<String> = sources
        .iter()
        .flat_map(|path| {
            let source = std::fs::read_to_string(path).unwrap();
            direct_secret_reads(&source)
                .into_iter()
                .map(move |name| format!("{}: {name}", path.display()))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "read these through hot::environment::read_optional so <NAME>_FILE works:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scan_recognizes_direct_reads() {
    let source = r#"
        let a = std::env::var("HOT_GOOGLE_CLIENT_SECRET");
        let b = env::var_os("HOT_DB_URI");
        let c = env::var("HOT_LOG_LEVEL");
        let d = env::var("HOT_TEST_POSTGRES_URI");
        std::env::set_var("HOT_ENCRYPTION_KEY", "x");
    "#;
    assert_eq!(
        direct_secret_reads(source),
        ["HOT_GOOGLE_CLIENT_SECRET", "HOT_DB_URI"]
    );
}
