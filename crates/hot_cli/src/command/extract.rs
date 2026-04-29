//! `hot extract` — extract a build artifact bundle to a directory.

use std::path::Path;

pub(crate) fn run_extract(
    build_path: &str,
    extract_dir: Option<&str>,
    build_dir: Option<&str>,
) -> Result<(), String> {
    let full_build_path = if Path::new(build_path).is_absolute() || build_path.contains('/') {
        build_path.to_string()
    } else {
        let build_dir = build_dir.unwrap_or(".hot/build");
        Path::new(build_dir)
            .join(build_path)
            .to_string_lossy()
            .to_string()
    };

    match hot::bundle::bundle_extract(&full_build_path, extract_dir) {
        Ok(extracted_dir) => {
            println!(
                "Bundle extracted successfully to: {}",
                extracted_dir.display()
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}
