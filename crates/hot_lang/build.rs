use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be available"),
    );
    let workspace_root = find_workspace_root(&manifest_dir);

    emit_source_fingerprint(&manifest_dir);

    // The release this crate belongs to, for packages' `hot-min-version`.
    // Same source as `hot`'s HOT_VERSION.
    println!(
        "cargo:rustc-env=HOT_VERSION={}",
        get_version(workspace_root.as_deref())
    );

    emit_embedded_std(workspace_root.as_deref());
}

// `hot` keys its AST and bytecode caches on a fingerprint of the compiler
// sources. The compiler lives here, so fingerprint this crate's sources too.
fn emit_source_fingerprint(manifest_dir: &Path) {
    let mut files = Vec::new();
    collect_files(&manifest_dir.join("src"), "rs", &mut files)
        .expect("failed to list hot_lang sources");
    files.push(manifest_dir.join("build.rs"));
    files.push(manifest_dir.join("Cargo.toml"));
    files.sort();

    let mut hasher = blake3::Hasher::new();
    for path in &files {
        let relative = path
            .strip_prefix(manifest_dir)
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

// hot-std's signatures are Hot source, so a checker needs hot-std's `.hot`
// files from the same commit as the compiler. Write a table of them for
// `lang::hot_std` to `include_str!`: `pkg.hot` and everything under `src/`,
// which is all package discovery reads.
fn emit_embedded_std(workspace_root: Option<&Path>) {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR must be available"));
    let std_dir = workspace_root.map(|root| root.join("hot/pkg/hot-std"));

    let mut files = Vec::new();
    if std::env::var_os("CARGO_FEATURE_EMBED_STD").is_some()
        && let Some(std_dir) = std_dir
            .as_deref()
            .filter(|dir| dir.join("pkg.hot").is_file())
    {
        files.push(std_dir.join("pkg.hot"));
        collect_files(&std_dir.join("src"), "hot", &mut files)
            .expect("failed to list hot-std sources");
        println!("cargo:rerun-if-changed={}", std_dir.display());
    }
    files.sort();

    let std_dir = std_dir.unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    let mut table = String::from("&[\n");
    for path in &files {
        let relative = path
            .strip_prefix(&std_dir)
            .expect("hot-std file is inside hot-std")
            .to_string_lossy()
            .replace('\\', "/");
        let bytes =
            fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
        hasher.update(relative.as_bytes());
        hasher.update(&[0]);
        hasher.update(&bytes);
        hasher.update(&[0]);
        table.push_str(&format!(
            "    ({:?}, include_str!({:?})),\n",
            relative,
            path.display().to_string()
        ));
    }
    table.push(']');
    fs::write(out_dir.join("hot_std_files.rs"), table)
        .expect("failed to write the embedded hot-std table");
    println!(
        "cargo:rustc-env=HOT_STD_FINGERPRINT={}",
        hasher.finalize().to_hex()
    );
}

fn find_workspace_root(manifest_dir: &Path) -> Option<PathBuf> {
    let package_manifest = fs::read_to_string(manifest_dir.join("Cargo.toml")).ok()?;
    if !package_manifest
        .lines()
        .any(|line| line.contains("workspace = true") || line.contains(".workspace = true"))
    {
        // Published/packaged manifests have workspace inheritance resolved.
        return None;
    }

    manifest_dir.ancestors().find_map(|candidate| {
        let manifest = fs::read_to_string(candidate.join("Cargo.toml")).ok()?;
        manifest
            .lines()
            .any(|line| line.trim() == "[workspace]")
            .then(|| candidate.to_path_buf())
    })
}

fn get_version(workspace_root: Option<&Path>) -> String {
    if let Some(workspace_root) = workspace_root {
        let version_file = workspace_root.join("resources/version.txt");
        println!("cargo:rerun-if-changed={}", version_file.display());
        if let Ok(version) = fs::read_to_string(version_file) {
            let version = version.trim();
            if !version.is_empty() {
                return version.to_string();
            }
        }
    }

    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string())
}

fn collect_files(dir: &Path, extension: &str, files: &mut Vec<PathBuf>) -> Result<(), String> {
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
            collect_files(&path, extension, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == extension) {
            files.push(path);
        }
    }
    Ok(())
}
