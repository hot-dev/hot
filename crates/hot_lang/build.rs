use std::fs;
use std::path::{Path, PathBuf};

// `hot` keys its AST and bytecode caches on a fingerprint of the compiler
// sources. The compiler lives here, so fingerprint this crate's sources too.
fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be available"),
    );
    let mut files = Vec::new();
    collect_rust_sources(&manifest_dir.join("src"), &mut files)
        .expect("failed to list hot_lang sources");
    files.push(manifest_dir.join("build.rs"));
    files.push(manifest_dir.join("Cargo.toml"));
    files.sort();

    let mut hasher = blake3::Hasher::new();
    for path in &files {
        let relative = path
            .strip_prefix(&manifest_dir)
            .expect("fingerprint input is inside the package");
        let bytes =
            fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(&bytes);
        hasher.update(&[0]);
    }
    println!(
        "cargo:rustc-env=HOT_LANG_SOURCE_FINGERPRINT={}",
        hasher.finalize().to_hex()
    );

    for path in ["src", "build.rs", "Cargo.toml"] {
        println!(
            "cargo:rerun-if-changed={}",
            manifest_dir.join(path).display()
        );
    }
}

fn collect_rust_sources(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("failed to read {}: {}", dir.display(), e))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("failed to read entry in {}: {}", dir.display(), e))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("failed to inspect {}: {}", path.display(), e))?;
        if file_type.is_dir() {
            collect_rust_sources(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}
