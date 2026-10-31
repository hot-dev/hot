//! Resolve file-backed environment settings without copying secrets into the
//! process environment (and therefore into user-code child processes).

use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Setting {
    pub name: String,
    pub value: String,
    pub source_file: Option<PathBuf>,
}

/// Read an environment setting, optionally supplied through `<NAME>_FILE`.
/// Errors deliberately contain variable names and paths, never file contents.
pub fn read_optional(name: &str) -> Result<Option<String>, String> {
    let mut bindings = BTreeMap::new();
    for key in [name.to_string(), format!("{name}_FILE")] {
        if let Some(value) = std::env::var_os(&key) {
            bindings.insert(
                key.clone(),
                value
                    .into_string()
                    .map_err(|_| format!("{key} must be UTF-8"))?,
            );
        }
    }
    resolve_one(name, &bindings).map(|setting| setting.map(|s| s.value))
}

/// Deterministic resolver, also used by the CLI's configuration overlay.
pub fn resolve_hot(bindings: BTreeMap<String, String>) -> Result<Vec<Setting>, String> {
    let names: std::collections::BTreeSet<_> = bindings
        .keys()
        .filter(|name| name.starts_with("HOT_"))
        .map(|name| name.strip_suffix("_FILE").unwrap_or(name).to_string())
        .collect();
    names
        .into_iter()
        .map(|name| resolve_one(&name, &bindings).map(Option::unwrap))
        .collect()
}

fn resolve_one(name: &str, bindings: &BTreeMap<String, String>) -> Result<Option<Setting>, String> {
    let file_name = format!("{name}_FILE");
    match (bindings.get(name), bindings.get(&file_name)) {
        (Some(_), Some(_)) => Err(format!("Set only one of {name} and {file_name}")),
        (Some(value), None) => Ok(Some(Setting {
            name: name.into(),
            value: value.clone(),
            source_file: None,
        })),
        (None, Some(path)) => {
            let contents = std::fs::read_to_string(path)
                .map_err(|e| format!("Cannot read {file_name} at {path:?}: {e}"))?;
            // Some editors save a UTF-8 byte order mark, which no value means.
            let contents = contents.strip_prefix('\u{feff}').unwrap_or(&contents);
            let value = contents
                .strip_suffix("\r\n")
                .or_else(|| contents.strip_suffix('\n'))
                .unwrap_or(contents);
            if value.is_empty() {
                return Err(format!("{file_name} at {path:?} is empty"));
            }
            Ok(Some(Setting {
                name: name.into(),
                value: value.into(),
                source_file: Some(path.into()),
            }))
        }
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bindings(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn file_values_preserve_spaces_and_remove_only_one_newline() {
        let path = std::env::temp_dir().join(format!("hot-secret-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "  private\n\n").unwrap();
        let resolved = resolve_hot(bindings(&[(
            "HOT_CUSTOM_VALUE_FILE",
            path.to_str().unwrap(),
        )]))
        .unwrap();
        assert_eq!(resolved[0].value, "  private\n");
        assert_eq!(resolved[0].name, "HOT_CUSTOM_VALUE");
        assert_eq!(resolved[0].source_file.as_ref(), Some(&path));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn conflict_errors_do_not_include_values() {
        let error = resolve_hot(bindings(&[
            ("HOT_DB_URI", "DO-NOT-PRINT"),
            ("HOT_DB_URI_FILE", "/unused"),
        ]))
        .err()
        .unwrap();
        assert!(error.contains("HOT_DB_URI_FILE"));
        assert!(!error.contains("DO-NOT-PRINT"));
    }

    #[test]
    fn unreadable_and_empty_files_fail_closed() {
        let path = std::env::temp_dir().join(format!("hot-secret-{}", uuid::Uuid::new_v4()));
        let settings = || bindings(&[("HOT_DB_URI_FILE", path.to_str().unwrap())]);
        assert!(resolve_hot(settings()).is_err());
        std::fs::write(&path, "\r\n").unwrap();
        assert!(resolve_hot(settings()).is_err());
        std::fs::write(&path, "private\r\n").unwrap();
        assert_eq!(resolve_hot(settings()).unwrap()[0].value, "private");
        std::fs::remove_file(path).unwrap();
    }
}
