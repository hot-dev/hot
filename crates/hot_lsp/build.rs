use std::fs;

fn main() {
    let version = if let Ok(version) = fs::read_to_string("../../resources/version.txt") {
        let version = version.trim();
        if !version.is_empty() {
            version.to_string()
        } else {
            "0.0.0".to_string()
        }
    } else {
        "0.0.0".to_string()
    };

    println!("cargo:rustc-env=HOT_VERSION={}", version);
    println!("cargo:rerun-if-changed=../../resources/version.txt");
}
