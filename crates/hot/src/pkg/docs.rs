//! Package documentation generator for Hot packages
//!
//! Parses .hot files and extracts documentation from metadata.
//! This module provides data structures and parsing - rendering is left to consumers.
//!
//! Uses the actual Hot parser for reliable AST-based extraction.

use crate::lang::ast::{Meta, NsPath, Ref, Value};
use crate::lang::parser::parse_hot_file;
use crate::val::Val;
use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Package dependency information
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PkgDep {
    /// Package name (derived from the key, e.g., "aws-core" from "hot.dev/aws-core")
    pub name: String,
    /// Git repository URL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// Path within the git repository
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Git tag
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Git branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Local path (for development)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// Link to local docs if this package is served (e.g., "/pkg/aws-core")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
}

/// Package metadata from pkg.hot
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PkgMeta {
    pub org: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub email: String,
    pub license: String,
    pub url: String,
    /// Tags for categorizing packages (e.g., "stripe", "aws", "ai")
    #[serde(default)]
    pub tags: Vec<String>,
    /// Package dependencies
    #[serde(default)]
    pub deps: Vec<PkgDep>,
}

/// Context variable requirements
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CtxRequirements {
    /// Required context variables
    #[serde(default)]
    pub req: Vec<String>,
    /// Optional context variables
    #[serde(default)]
    pub opt: Vec<String>,
}

/// Container (box) requirement for a function or namespace
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BoxRequirementDoc {
    /// Minimum container size preset (e.g. "nano", "small", "medium")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<String>,
    /// Whether network access is required
    #[serde(default)]
    pub network: bool,
}

/// Webhook metadata summary for documentation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DocWebhook {
    pub service: String,
    pub path: String,
}

/// MCP tool metadata summary for documentation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DocMcp {
    pub service: String,
    pub name: String,
}

/// A documented function from a Hot source file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocFunction {
    pub name: String,
    pub doc: Option<String>,
    pub is_core: bool,
    pub signatures: Vec<FunctionSignature>,
    /// Context variable requirements for this function
    #[serde(default)]
    pub ctx: Option<CtxRequirements>,
    /// Container (box) requirements for this function
    #[serde(default)]
    pub box_req: Option<BoxRequirementDoc>,
    /// Schedule cron expression (from meta {schedule: "..."})
    #[serde(default)]
    pub schedule: Option<String>,
    /// Event handler type (from meta {on-event: "..."})
    #[serde(default)]
    pub on_event: Option<String>,
    /// Webhook configuration (from meta {webhook: {service: ..., path: ...}})
    #[serde(default)]
    pub webhook: Option<DocWebhook>,
    /// MCP tool configuration (from meta {mcp: {service: ..., name: ...}})
    #[serde(default)]
    pub mcp: Option<DocMcp>,
    /// Event names this function sends (from meta {sends: [...]} and static extraction)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<String>,
    /// Set when this entry is a var alias to another function — covers
    /// both plain re-exports (`re-exported ::lib/utility`) and the
    /// wrapper-pattern (`tg-record-voice meta {...} ::tg-adapter/record-voice`).
    /// Holds the fully-qualified name the alias points at so the
    /// renderer can link out to the underlying implementation. `None`
    /// for ordinary `Value::Fn` definitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_of: Option<String>,
}

/// A function signature (supports overloads)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSignature {
    pub params: Vec<FunctionParam>,
    pub return_type: Option<String>,
}

/// A function parameter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionParam {
    pub name: String,
    pub type_annotation: Option<String>,
    pub is_lazy: bool,
    pub is_variadic: bool,
}

/// A documented type from a Hot source file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocType {
    pub name: String,
    pub doc: Option<String>,
    pub is_core: bool,
    pub fields: Vec<TypeField>,
    pub constructors: Vec<FunctionSignature>,
    /// For literal union types like `"user" | "assistant"`, the type expression as a string
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_alias: Option<String>,
}

/// A field in a type definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeField {
    pub name: String,
    pub type_annotation: Option<String>,
}

/// A documented namespace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocNamespace {
    pub name: String,        // e.g., "hot/str" (used for URL paths)
    pub namespace: String,   // e.g., "::hot::str"
    pub doc: Option<String>, // Namespace documentation from ns declaration metadata
    pub no_doc: bool,        // If true, exclude from documentation
    pub functions: Vec<DocFunction>,
    pub types: Vec<DocType>,
    /// Context variable requirements for this namespace
    #[serde(default)]
    pub ctx: Option<CtxRequirements>,
    /// Aggregate container (box) requirements for this namespace
    #[serde(default)]
    pub box_req: Option<BoxRequirementDoc>,
}

/// Complete package documentation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgDocs {
    pub meta: PkgMeta,
    #[serde(default)]
    pub readme: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    pub namespaces: Vec<DocNamespace>,
    /// Type index: maps type names to their URL paths (e.g., "S3Bucket" -> "aws/s3/buckets")
    /// Used for cross-namespace type linking within the package
    #[serde(default)]
    pub type_index: AHashMap<String, String>,
}

/// Index of available versions for a package
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgVersionsIndex {
    /// The latest/current version
    pub latest: String,
    /// All available versions, sorted newest first
    pub versions: Vec<String>,
}

/// Find hot-std package path using standard resolution
pub fn find_hot_std_path() -> Option<PathBuf> {
    // 1. HOT_HOME environment variable
    if let Ok(home) = std::env::var("HOT_HOME") {
        let path = PathBuf::from(home).join("pkg").join("hot-std");
        if path.exists() {
            return Some(path);
        }
    }

    // 2. Development/local path
    let dev_path = PathBuf::from("./hot/pkg/hot-std");
    if dev_path.exists() {
        return Some(dev_path);
    }

    // 3. Executable-relative
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let exe_relative = exe_dir.join("resources").join("pkg").join("hot-std");
        if exe_relative.exists() {
            return Some(exe_relative);
        }
    }

    // 4. macOS package install
    let macos_path = PathBuf::from("/usr/local/share/hot/pkg/hot-std");
    if macos_path.exists() {
        return Some(macos_path);
    }

    // 5. Linux package install
    let linux_path = PathBuf::from("/usr/share/hot/pkg/hot-std");
    if linux_path.exists() {
        return Some(linux_path);
    }

    None
}

/// Find a package by name
pub fn find_pkg_path(pkg_name: &str) -> Option<PathBuf> {
    // Special case for hot-std (uses dedicated discovery)
    if pkg_name == "hot-std" {
        return find_hot_std_path();
    }

    // 1. HOT_HOME environment variable
    if let Ok(home) = std::env::var("HOT_HOME") {
        let path = PathBuf::from(home).join("pkg").join(pkg_name);
        if path.exists() && path.join("pkg.hot").exists() {
            return Some(path);
        }
    }

    // 2. Development/local path
    let dev_path = PathBuf::from("./hot/pkg").join(pkg_name);
    if dev_path.exists() && dev_path.join("pkg.hot").exists() {
        return Some(dev_path);
    }

    // 3. Executable-relative
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let exe_relative = exe_dir.join("resources").join("pkg").join(pkg_name);
        if exe_relative.exists() && exe_relative.join("pkg.hot").exists() {
            return Some(exe_relative);
        }
    }

    // 4. macOS package install
    let macos_path = PathBuf::from("/usr/local/share/hot/pkg").join(pkg_name);
    if macos_path.exists() && macos_path.join("pkg.hot").exists() {
        return Some(macos_path);
    }

    // 5. Linux package install
    let linux_path = PathBuf::from("/usr/share/hot/pkg").join(pkg_name);
    if linux_path.exists() && linux_path.join("pkg.hot").exists() {
        return Some(linux_path);
    }

    None
}

/// Summary info for a package (lighter than full PkgDocs)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgSummary {
    /// Full package name including org (e.g., "hot.dev/anthropic") - used for URLs
    pub name: String,
    /// Short display name without org (e.g., "anthropic") - used for display
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub tags: Vec<String>,
    pub namespace_count: usize,
    pub function_count: usize,
    pub type_count: usize,
}

impl PkgSummary {
    /// Extract display_name from full name (e.g., "hot.dev/anthropic" -> "anthropic")
    pub fn extract_display_name(name: &str) -> String {
        name.split('/').next_back().unwrap_or(name).to_string()
    }
}

/// List all available packages with summary information
pub fn list_all_packages() -> Vec<PkgSummary> {
    tracing::debug!("list_all_packages: starting package discovery");
    let mut packages = Vec::new();

    // Check if we should use source packages (HOT_REBUILD_DOCS=true means use source)
    // When HOT_REBUILD_DOCS is not set or set to false, we skip source directories
    // and only use pre-generated docs. This allows local testing of the deployed behavior.
    let use_source_packages = std::env::var("HOT_REBUILD_DOCS")
        .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
        .unwrap_or(false);

    if use_source_packages {
        // Check the dev path first (for development)
        let dev_pkg_dir = PathBuf::from("./hot/pkg");
        tracing::debug!(
            "Checking dev pkg dir: {:?} (exists: {})",
            dev_pkg_dir,
            dev_pkg_dir.exists()
        );
        if dev_pkg_dir.exists()
            && let Ok(entries) = fs::read_dir(&dev_pkg_dir)
        {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir()
                    && path.join("pkg.hot").exists()
                    && let Some(summary) = load_pkg_summary(&path)
                {
                    packages.push(summary);
                }
            }
        }

        // Also check HOT_HOME/pkg if set
        if let Ok(home) = std::env::var("HOT_HOME") {
            let home_pkg_dir = PathBuf::from(home).join("pkg");
            if home_pkg_dir.exists()
                && home_pkg_dir != dev_pkg_dir
                && let Ok(entries) = fs::read_dir(&home_pkg_dir)
            {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.is_dir() && path.join("pkg.hot").exists() {
                        // Only add if not already in the list
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if !packages.iter().any(|p| p.name == name)
                            && let Some(summary) = load_pkg_summary(&path)
                        {
                            packages.push(summary);
                        }
                    }
                }
            }
        }
    } else {
        tracing::debug!("HOT_REBUILD_DOCS not set, skipping source directories");
    }

    // Also check pre-generated docs (for deployed environments without source packages)
    match crate::resources::get_pkg_docs_path() {
        Ok(docs_dir) => {
            tracing::debug!("Checking pkg-docs at: {:?}", docs_dir);
            if docs_dir.exists() {
                match fs::read_dir(&docs_dir) {
                    Ok(entries) => {
                        for entry in entries.filter_map(|e| e.ok()) {
                            let path = entry.path();
                            if path.is_dir() {
                                let pkg_name = match path.file_name().and_then(|n| n.to_str()) {
                                    Some(name) => name,
                                    None => continue,
                                };

                                // Skip if already discovered from source
                                if packages.iter().any(|p| p.name == pkg_name) {
                                    tracing::debug!(
                                        "Skipping {} (already found from source)",
                                        pkg_name
                                    );
                                    continue;
                                }

                                // Try to load summary from pre-generated docs
                                match load_pkg_summary_from_docs(&path, pkg_name) {
                                    Some(summary) => {
                                        tracing::debug!("Loaded pkg from docs: {}", pkg_name);
                                        packages.push(summary);
                                    }
                                    None => {
                                        tracing::warn!(
                                            "Failed to load pkg summary from docs: {}",
                                            pkg_name
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read pkg-docs dir {:?}: {}", docs_dir, e);
                    }
                }
            } else {
                tracing::warn!("pkg-docs dir does not exist: {:?}", docs_dir);
            }
        }
        Err(e) => {
            tracing::debug!("Could not get pkg-docs path: {}", e);
        }
    }

    // Sort: hot-std first, then alphabetically by display_name
    packages.sort_by(|a, b| {
        if a.display_name == "hot-std" {
            std::cmp::Ordering::Less
        } else if b.display_name == "hot-std" {
            std::cmp::Ordering::Greater
        } else {
            a.display_name.cmp(&b.display_name)
        }
    });

    tracing::info!("list_all_packages: found {} packages", packages.len());
    packages
}

/// Load package summary from pre-generated docs (for deployed environments)
fn load_pkg_summary_from_docs(pkg_docs_path: &Path, pkg_name: &str) -> Option<PkgSummary> {
    // Read versions.json to find the latest version
    let versions_path = pkg_docs_path.join("versions.json");
    let versions_json = match fs::read_to_string(&versions_path) {
        Ok(json) => json,
        Err(e) => {
            tracing::debug!(
                "Failed to read versions.json for {}: {} (path: {:?})",
                pkg_name,
                e,
                versions_path
            );
            return None;
        }
    };
    let versions_index: PkgVersionsIndex = match serde_json::from_str(&versions_json) {
        Ok(idx) => idx,
        Err(e) => {
            tracing::debug!("Failed to parse versions.json for {}: {}", pkg_name, e);
            return None;
        }
    };

    // Load the docs.json for the latest version
    let docs_path = pkg_docs_path.join(&versions_index.latest).join("docs.json");
    let docs = match load_pkg_docs_from_json(&docs_path) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(
                "Failed to load docs.json for {}: {} (path: {:?})",
                pkg_name,
                e,
                docs_path
            );
            return None;
        }
    };

    let ns_count = docs.namespaces.len();
    let fn_count: usize = docs.namespaces.iter().map(|n| n.functions.len()).sum();
    let ty_count: usize = docs.namespaces.iter().map(|n| n.types.len()).sum();

    // Use full name from metadata (e.g., "hot.dev/anthropic") for URLs
    let full_name = docs.meta.name.clone();
    let display_name = PkgSummary::extract_display_name(&full_name);

    Some(PkgSummary {
        name: full_name,
        display_name,
        version: docs.meta.version,
        description: docs.meta.description,
        tags: docs.meta.tags,
        namespace_count: ns_count,
        function_count: fn_count,
        type_count: ty_count,
    })
}

/// Load summary information for a package (fast, doesn't parse all namespaces)
fn load_pkg_summary(pkg_path: &Path) -> Option<PkgSummary> {
    let pkg_hot_path = pkg_path.join("pkg.hot");
    let meta = parse_pkg_hot(&pkg_hot_path).ok()?;

    // Try to load pre-generated docs to get counts
    let pkg_name = pkg_path.file_name()?.to_str()?;

    // Try to find pre-generated docs
    let docs_path = crate::resources::get_pkg_docs_path().ok();
    let (namespace_count, function_count, type_count) = if let Some(docs_dir) = docs_path {
        let version_docs_path = docs_dir
            .join(pkg_name)
            .join(&meta.version)
            .join("docs.json");
        if version_docs_path.exists() {
            if let Ok(docs) = load_pkg_docs_from_json(&version_docs_path) {
                let ns_count = docs.namespaces.len();
                let fn_count: usize = docs.namespaces.iter().map(|n| n.functions.len()).sum();
                let ty_count: usize = docs.namespaces.iter().map(|n| n.types.len()).sum();
                (ns_count, fn_count, ty_count)
            } else {
                (0, 0, 0)
            }
        } else {
            (0, 0, 0)
        }
    } else {
        (0, 0, 0)
    };

    let display_name = PkgSummary::extract_display_name(&meta.name);

    Some(PkgSummary {
        name: meta.name,
        display_name,
        version: meta.version,
        description: meta.description,
        tags: meta.tags,
        namespace_count,
        function_count,
        type_count,
    })
}

/// Load package documentation from a package directory
pub fn load_pkg_docs(pkg_path: &Path) -> Result<PkgDocs, String> {
    // Load pkg.hot
    let pkg_hot_path = pkg_path.join("pkg.hot");
    let meta = parse_pkg_hot(&pkg_hot_path)?;

    // Load README.md if present
    let readme_path = pkg_path.join("README.md");
    let readme = if readme_path.exists() {
        fs::read_to_string(&readme_path).ok()
    } else {
        None
    };

    // Load LICENSE if present (try common license file names)
    let license = ["LICENSE", "LICENSE.md", "LICENSE.txt", "LICENCE"]
        .iter()
        .map(|name| pkg_path.join(name))
        .find(|p| p.exists())
        .and_then(|p| fs::read_to_string(p).ok());

    // Find and parse all source files
    let src_path = pkg_path.join("src");
    let modules = if src_path.exists() {
        discover_namespaces(&src_path)?
    } else {
        Vec::new()
    };

    // Build type index: map each type name to its namespace URL path
    let type_index = build_type_index(&modules);

    Ok(PkgDocs {
        meta,
        readme,
        license,
        namespaces: modules,
        type_index,
    })
}

/// Build an index of all types in the package, mapping type names to their namespace URL paths
fn build_type_index(namespaces: &[DocNamespace]) -> AHashMap<String, String> {
    let mut index = AHashMap::new();

    for ns in namespaces {
        for typ in &ns.types {
            // Map type name to namespace URL path (e.g., "S3Bucket" -> "aws/s3/buckets")
            index.insert(typ.name.clone(), ns.name.clone());
        }
    }

    index
}

/// Parse pkg.hot file for metadata
fn parse_pkg_hot(path: &Path) -> Result<PkgMeta, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("Failed to read pkg.hot: {}", e))?;

    let mut meta = PkgMeta::default();

    // Find the JSON-like block
    if let Some(brace_start) = content.find('{')
        && let Some(brace_end) = content.rfind('}')
    {
        let json_content = &content[brace_start..=brace_end];

        // Parse key-value pairs
        for line in json_content.lines() {
            let line = line.trim();
            if let Some((key, value)) = parse_key_value(line) {
                match key.as_str() {
                    "org" => meta.org = value,
                    "name" => meta.name = value,
                    "version" => meta.version = value,
                    "description" => meta.description = value,
                    "author" => meta.author = value,
                    "email" => meta.email = value,
                    "license" => meta.license = value,
                    "url" => meta.url = value,
                    _ => {}
                }
            }
        }

        // Parse tags array separately (it spans a single line like: tags: ["stripe", "payments"])
        meta.tags = parse_string_array(json_content, "tags");

        // Parse deps block
        meta.deps = parse_deps_block(json_content);
    }

    // If org is empty but name contains a slash, derive org from name
    // e.g., "hot.dev/anthropic" -> org: "hot.dev", name stays as-is
    if meta.org.is_empty()
        && let Some(slash_pos) = meta.name.find('/')
    {
        meta.org = meta.name[..slash_pos].to_string();
    }

    Ok(meta)
}

/// Parse a string array from content like: key: ["value1", "value2"]
fn parse_string_array(content: &str, key: &str) -> Vec<String> {
    let mut result = Vec::new();

    // Look for pattern: key: [...]
    for line in content.lines() {
        let line = line.trim();
        // Match both "key": [...] and key: [...]
        let key_pattern1 = format!("\"{}\": [", key);
        let key_pattern2 = format!("{}: [", key);

        let start_idx = if let Some(idx) = line.find(&key_pattern1) {
            Some(idx + key_pattern1.len() - 1)
        } else {
            line.find(&key_pattern2)
                .map(|idx| idx + key_pattern2.len() - 1)
        };

        if let Some(start) = start_idx {
            // Find the closing bracket
            if let Some(end) = line[start..].find(']') {
                let array_content = &line[start + 1..start + end];
                // Parse comma-separated quoted strings
                for item in array_content.split(',') {
                    let item = item.trim().trim_matches('"');
                    if !item.is_empty() {
                        result.push(item.to_string());
                    }
                }
            }
        }
    }

    result
}

/// Parse the deps block from pkg.hot content
/// Handles nested braces like: deps: { "hot.dev/aws-core": { git: "...", path: "..." } }
fn parse_deps_block(content: &str) -> Vec<PkgDep> {
    let mut deps = Vec::new();

    // Find "deps:" or "deps :" followed by an opening brace
    let deps_start = content.find("deps:").or_else(|| content.find("deps :"));

    let Some(deps_pos) = deps_start else {
        return deps;
    };

    // Find the opening brace after deps:
    let after_deps = &content[deps_pos..];
    let Some(brace_start) = after_deps.find('{') else {
        return deps;
    };

    // Find the matching closing brace (handle nesting)
    let deps_content_start = deps_pos + brace_start + 1;
    let mut depth = 1;
    let mut brace_end = deps_content_start;
    for (i, c) in content[deps_content_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    brace_end = deps_content_start + i;
                    break;
                }
            }
            _ => {}
        }
    }

    let deps_block = &content[deps_content_start..brace_end];

    // Parse each dependency entry
    // Pattern: "hot.dev/pkg-name": { ... } or "hot.dev/pkg-name": {}
    let mut i = 0;
    let chars: Vec<char> = deps_block.chars().collect();
    while i < chars.len() {
        // Skip whitespace
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }

        // Look for quoted key
        if chars[i] == '"' {
            i += 1;
            let key_start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let key: String = chars[key_start..i].iter().collect();
            i += 1; // skip closing quote

            // Extract package name from key (e.g., "hot.dev/aws-core" -> "aws-core")
            let pkg_name = key.rsplit('/').next().unwrap_or(&key).to_string();

            // Skip to the colon and opening brace
            while i < chars.len() && chars[i] != '{' {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            i += 1; // skip opening brace

            // Find matching closing brace for this dep's value
            let value_start = i;
            let mut value_depth = 1;
            while i < chars.len() && value_depth > 0 {
                match chars[i] {
                    '{' => value_depth += 1,
                    '}' => value_depth -= 1,
                    _ => {}
                }
                if value_depth > 0 {
                    i += 1;
                }
            }
            let value_content: String = chars[value_start..i].iter().collect();
            i += 1; // skip closing brace

            // Parse the dep's properties
            let mut dep = PkgDep {
                name: pkg_name,
                ..Default::default()
            };

            // Extract key-value pairs from the dep object
            for line in value_content.lines() {
                let line = line.trim();
                if let Some((k, v)) = parse_dep_key_value(line) {
                    match k.as_str() {
                        "git" => dep.git = Some(v),
                        "path" => dep.path = Some(v),
                        "tag" => dep.tag = Some(v),
                        "branch" => dep.branch = Some(v),
                        "local" => dep.local = Some(v),
                        _ => {}
                    }
                }
            }

            deps.push(dep);
        } else {
            i += 1;
        }
    }

    deps
}

/// Parse a key-value pair from a dep object line like: "git": "https://..." or git: "https://..."
fn parse_dep_key_value(line: &str) -> Option<(String, String)> {
    let line = line.trim().trim_end_matches(',');

    // Try pattern: "key": "value" or key: "value"
    let colon_pos = line.find(':')?;
    let key = line[..colon_pos].trim().trim_matches('"').to_string();
    let value_part = line[colon_pos + 1..].trim();

    // Extract quoted value
    if value_part.starts_with('"') {
        let value_content = value_part.trim_matches('"');
        Some((key, value_content.to_string()))
    } else {
        None
    }
}

/// Find the position of the first unescaped quote in a string
fn find_unescaped_quote(s: &str) -> Option<usize> {
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'"' {
            // Check if this quote is escaped by counting preceding backslashes
            let mut backslash_count = 0;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                backslash_count += 1;
                j -= 1;
            }
            // If even number of backslashes, the quote is not escaped
            if backslash_count % 2 == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Process escape sequences in a string (like the Hot lexer does)
fn unescape_string(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('\'') => result.push('\''),
                Some('0') => result.push('\0'),
                Some('`') => result.push('`'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Parse a "key": "value" line
fn parse_key_value(line: &str) -> Option<(String, String)> {
    let line = line.trim().trim_end_matches(',');

    // Skip empty lines, comments, and lines without colons
    if line.is_empty() || line.starts_with("//") || !line.contains(':') {
        return None;
    }

    // Handle both quoted keys ("name": "value") and unquoted keys (name: "value")
    let mut key = String::new();
    let mut value = String::new();
    let mut in_quoted_key = false;
    let mut in_value = false;
    let mut found_colon = false;
    let mut chars = line.chars().peekable();

    // Parse key (either quoted or unquoted identifier)
    while let Some(&ch) = chars.peek() {
        if ch == '"' {
            chars.next();
            in_quoted_key = !in_quoted_key;
            if !in_quoted_key {
                // End of quoted key
                break;
            }
        } else if ch == ':' && !in_quoted_key {
            // End of unquoted key
            break;
        } else if in_quoted_key || ch.is_alphanumeric() || ch == '_' || ch == '-' {
            key.push(ch);
            chars.next();
        } else if ch.is_whitespace() && key.is_empty() {
            chars.next(); // Skip leading whitespace
        } else {
            chars.next(); // Skip other characters before key
        }
    }

    // Find and skip the colon
    for ch in chars.by_ref() {
        if ch == ':' {
            found_colon = true;
            break;
        }
    }

    if !found_colon {
        return None;
    }

    // Parse value (quoted string)
    for ch in chars {
        if ch == '"' {
            in_value = !in_value;
        } else if in_value {
            value.push(ch);
        }
    }

    if !key.is_empty() {
        Some((key, value))
    } else {
        None
    }
}

/// Discover all documented namespaces under `src_path`.
///
/// Discovery routes through `crate::discovery::discover` so package-doc
/// generation honors `.gitignore` / `.hotignore` / default hard-excludes
/// uniformly with the rest of the toolchain.
fn discover_namespaces(src_path: &Path) -> Result<Vec<DocNamespace>, String> {
    tracing::info!("Discovering namespaces in: {}", src_path.display());
    let mut namespaces = Vec::new();

    let opts = crate::discovery::DiscoveryOpts::for_extension("hot");
    for found in crate::discovery::discover(&[src_path], &opts) {
        let path = found.abs_path;
        tracing::debug!("Parsing file: {}", path.display());
        // Try AST-based parser first (more reliable), fall back to text-based.
        let ns_result = parse_namespace_from_ast(&path).or_else(|ast_err| {
            tracing::info!("AST parser failed for {}: {}", path.display(), ast_err);
            parse_namespace_from_text(&path)
        });

        match &ns_result {
            Ok(ns) => {
                if ns.no_doc {
                    tracing::info!("Skipping namespace {} (no-doc: true)", ns.namespace);
                } else if !ns.functions.is_empty() || !ns.types.is_empty() {
                    tracing::info!(
                        "Found namespace {} with {} functions, {} types",
                        ns.namespace,
                        ns.functions.len(),
                        ns.types.len()
                    );
                    namespaces.push(ns.clone());
                } else {
                    tracing::info!(
                        "Skipping empty namespace {} from {}",
                        ns.namespace,
                        path.display()
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", path.display(), e);
            }
        }
    }

    tracing::info!("Discovered {} namespaces", namespaces.len());
    namespaces.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(namespaces)
}

/// Parse a .hot file for documentation (text-based fallback)
fn parse_namespace_from_text(path: &Path) -> Result<DocNamespace, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let mut namespace = String::new();
    let mut namespace_doc = None;
    let mut functions = Vec::new();
    let mut types = Vec::new();

    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        // Parse namespace declaration with optional metadata
        // New syntax formats: ::namespace ns
        //                     ::namespace meta {doc: "..."} ns
        if line.starts_with("::") && (line.ends_with(" ns") || line.contains("#")) {
            // Extract namespace path (starts with :: and ends before metadata or 'ns')
            let ns_end = if let Some(meta_pos) = line.find('#') {
                meta_pos
            } else if let Some(ns_pos) = line.rfind(" ns") {
                ns_pos
            } else {
                line.len()
            };
            let ns_path = line[..ns_end].trim();
            namespace = ns_path.to_string();

            // Check for metadata
            if let Some(_meta_start) = line.find('#') {
                // Parse namespace metadata
                let (doc, _, meta_end) = parse_metadata(&lines, i);
                namespace_doc = doc;
                i = meta_end + 1;
            } else {
                i += 1;
            }
            continue;
        }

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with("//") {
            i += 1;
            continue;
        }

        // Try to parse a function or type definition
        if let Some((item, end_line)) = try_parse_item(&lines, i) {
            match item {
                ParsedItem::Function(func) => functions.push(*func),
                ParsedItem::Type(typ) => types.push(typ),
            }
            i = end_line + 1;
        } else {
            i += 1;
        }
    }

    // Derive name from namespace (full path with / separators, e.g., "hot/math" from "::hot::math")
    let name = namespace
        .split("::")
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");

    Ok(DocNamespace {
        name,
        namespace,
        doc: namespace_doc,
        no_doc: false, // Text parser doesn't support no-doc; use AST parser
        functions,
        types,
        ctx: None,     // Text parser doesn't support ctx; use AST parser
        box_req: None, // Text parser doesn't support box; use AST parser
    })
}

/// Parse a .hot file using the actual Hot parser (AST-based, more reliable)
/// This is the preferred method as it reuses the parser used by the LSP and compiler.
pub fn parse_namespace_from_ast(path: &Path) -> Result<DocNamespace, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let program = parse_hot_file(&content, path.to_string_lossy().as_ref())
        .map_err(|e| format!("Failed to parse {}: {:?}", path.display(), e))?;

    // Get the first (and usually only) namespace from the parsed program
    // Skip the default namespace if it exists and get the actual one
    let (ns_path, ns) = program
        .namespaces
        .iter()
        .find(|(path, _)| !path.is_empty() && path.to_string() != "::")
        .or_else(|| program.namespaces.iter().next())
        .ok_or_else(|| format!("No namespace found in {}", path.display()))?;

    let namespace = ns_path.to_string();
    let namespace_doc = extract_doc_from_meta(&ns.meta);
    let namespace_no_doc = is_no_doc_from_meta(&ns.meta);
    let namespace_ctx = extract_ctx_from_meta(&ns.meta);

    let mut functions = Vec::new();
    let mut types = Vec::new();

    // Build a single resolver up front so all type strings get the same
    // alias/var-import treatment.
    let resolver = TypeResolver::from_namespace(ns);

    // Iterate over all vars in the namespace
    for (var, value) in &ns.scope.vars {
        let var_name = var.sym.name().to_string();
        let var_meta = &var.meta;

        // Skip items marked with no-doc
        if is_no_doc_from_meta(var_meta) {
            continue;
        }

        match value {
            Value::TypeDef(type_def) => {
                let doc = extract_doc_from_meta(var_meta);
                let is_core = is_core_from_meta(var_meta);

                // Extract fields from the AST
                let fields = type_def
                    .fields
                    .as_ref()
                    .map(|f| {
                        f.iter()
                            .map(|field| TypeField {
                                name: field.name.name().to_string(),
                                type_annotation: Some(clean_type_annotation(
                                    &field.type_annotation,
                                    &resolver,
                                )),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Extract constructor functions
                let constructors = type_def
                    .constructor_functions
                    .as_ref()
                    .map(|ctors| {
                        ctors
                            .iter()
                            .map(|fn_def| FunctionSignature {
                                params: fn_def
                                    .args
                                    .args
                                    .iter()
                                    .map(|arg| FunctionParam {
                                        name: arg.var.sym.name().to_string(),
                                        type_annotation: arg
                                            .type_annotation
                                            .as_ref()
                                            .map(|t| clean_type_annotation(t, &resolver)),
                                        is_lazy: arg.lazy,
                                        is_variadic: false,
                                    })
                                    .collect(),
                                return_type: Some(var_name.clone()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Extract type alias (for literal unions like "user" | "assistant")
                let type_alias = type_def
                    .type_alias
                    .as_ref()
                    .map(|ta| clean_type_annotation(&ta.to_string(), &resolver));

                types.push(DocType {
                    name: var_name,
                    doc,
                    is_core,
                    fields,
                    constructors,
                    type_alias,
                });
            }
            Value::Fn(fn_defs) => {
                // Fn contains Vec<FnDef> for overloaded functions
                // Metadata is on the var, not on FnDef
                let doc = extract_doc_from_meta(var_meta);
                let is_core = is_core_from_meta(var_meta);
                let ctx = extract_ctx_from_meta(var_meta);
                let box_req = extract_box_from_meta(var_meta);
                let schedule = extract_schedule_from_meta(var_meta);
                let on_event = extract_on_event_from_meta(var_meta);
                let webhook = extract_webhook_from_meta(var_meta);
                let mcp = extract_mcp_from_meta(var_meta);
                let sends = extract_sends_from_meta(var_meta);

                let signatures = fn_defs
                    .iter()
                    .map(|fn_def| FunctionSignature {
                        params: fn_def
                            .args
                            .args
                            .iter()
                            .map(|arg| FunctionParam {
                                name: arg.var.sym.name().to_string(),
                                type_annotation: arg
                                    .type_annotation
                                    .as_ref()
                                    .map(|t| clean_type_annotation(t, &resolver)),
                                is_lazy: arg.lazy,
                                is_variadic: false,
                            })
                            .collect(),
                        return_type: fn_def
                            .return_type
                            .as_ref()
                            .map(|t| clean_type_annotation(t, &resolver)),
                    })
                    .collect();

                functions.push(DocFunction {
                    name: var_name,
                    doc,
                    is_core,
                    signatures,
                    ctx,
                    box_req,
                    schedule,
                    on_event,
                    webhook,
                    mcp,
                    sends,
                    alias_of: None,
                });
            }
            Value::Ref(ref_value) => {
                // Var aliases are a first-class language feature, not
                // just a wrapper-pattern affordance. Surface them in
                // package docs so plain re-exports
                // (`re-exported ::lib/utility`) and meta-bearing
                // wrappers (`tg-record-voice meta {...} ::tg/record-voice`)
                // both appear with the alias's name as a documented
                // entity that links out to the target.
                //
                // We synthesize an effective meta by merging the
                // target's meta under the alias's (alias keys win) —
                // same semantics used by the compiler artifact
                // extractors. Practical effect: plain aliases inherit
                // `doc`/`ctx`/etc. from the target without the user
                // having to redeclare them; meta-bearing aliases keep
                // their overrides while filling in unspecified fields
                // from the target.
                //
                // Signatures and target-meta lookups are best-effort:
                // they only resolve when the target lives in the same
                // parsed file. Cross-file targets get the alias's own
                // meta + an `alias_of` link to the target's docs page.
                let target_var = find_alias_target_var(ref_value, &program, &namespace);
                let target_meta = target_var.and_then(|v| v.meta.as_ref());
                let effective_meta = merge_alias_meta_for_docs(target_meta, var_meta.as_ref());

                let doc = extract_doc_from_meta(&effective_meta);
                let is_core = is_core_from_meta(&effective_meta);
                let ctx = extract_ctx_from_meta(&effective_meta);
                let box_req = extract_box_from_meta(&effective_meta);
                let schedule = extract_schedule_from_meta(&effective_meta);
                let on_event = extract_on_event_from_meta(&effective_meta);
                let webhook = extract_webhook_from_meta(&effective_meta);
                let mcp = extract_mcp_from_meta(&effective_meta);
                let sends = extract_sends_from_meta(&effective_meta);

                let alias_of = format_ref_target(ref_value, &namespace);
                let signatures = signatures_from_ref(ref_value, &program, &namespace, &resolver);

                functions.push(DocFunction {
                    name: var_name,
                    doc,
                    is_core,
                    signatures,
                    ctx,
                    box_req,
                    schedule,
                    on_event,
                    webhook,
                    mcp,
                    sends,
                    alias_of: Some(alias_of),
                });
            }
            _ => {
                // Skip other value types (literals, etc.)
            }
        }
    }

    // Derive name from namespace (full path with / separators, e.g., "hot/math" from "::hot::math")
    let name = namespace
        .split("::")
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");

    // Aggregate box requirements across all functions in this namespace
    let namespace_box_req = {
        use crate::lang::compiler::box_checker::size_gte;
        let mut has_any = false;
        let mut max_size: Option<String> = None;
        let mut needs_network = false;
        for f in &functions {
            if let Some(ref br) = f.box_req {
                has_any = true;
                if let Some(ref sz) = br.min_size {
                    match &max_size {
                        Some(current) if size_gte(current, sz) => {}
                        _ => max_size = Some(sz.clone()),
                    }
                }
                if br.network {
                    needs_network = true;
                }
            }
        }
        if has_any {
            Some(BoxRequirementDoc {
                min_size: max_size,
                network: needs_network,
            })
        } else {
            None
        }
    };

    Ok(DocNamespace {
        name,
        namespace,
        doc: namespace_doc,
        no_doc: namespace_no_doc,
        functions,
        types,
        ctx: namespace_ctx,
        box_req: namespace_box_req,
    })
}

/// Extract doc string from metadata
fn extract_doc_from_meta(meta: &Option<Meta>) -> Option<String> {
    meta.as_ref().and_then(|m| {
        m.val.get("doc").and_then(|v| match v {
            Val::Str(s) => Some(s.trim().to_string()),
            _ => None,
        })
    })
}

/// Find the `Var` an alias points at within the same parsed `Program`.
/// Returns `None` for cross-file targets — those are linked via
/// `alias_of` rather than inlined into the docs.
///
/// Single hop: if the immediate target is itself an alias, this returns
/// that intermediate alias's `Var` rather than chasing the chain. That
/// matches the single-hop semantics of `signatures_from_ref` so the
/// effective meta and signature stay in sync.
fn find_alias_target_var<'a>(
    ref_value: &'a Ref,
    program: &'a crate::lang::ast::Program,
    current_namespace: &str,
) -> Option<&'a crate::lang::ast::Var> {
    let (target_ns, target_fn) = match ref_value {
        Ref::Ns(ns_ref) => (
            ns_ref.ns.to_string(),
            ns_ref.function_name.as_deref()?.to_string(),
        ),
        Ref::Var(var_ref) => (
            current_namespace.to_string(),
            var_ref.var.sym.name().to_string(),
        ),
    };
    let (_, namespace) = program
        .namespaces
        .iter()
        .find(|(p, _)| p.to_string() == target_ns)?;
    namespace
        .scope
        .vars
        .iter()
        .find(|(v, _)| v.sym.name() == target_fn)
        .map(|(v, _)| v)
}

/// Build the effective `Meta` for an alias by merging the target's
/// meta under the alias's (alias keys win on collision). Both maps
/// are optional. Mirrors `merge_alias_meta` in
/// `compiler/artifact_extraction.rs` but kept local to docs to avoid
/// crossing the doc-gen ↔ compiler module boundary.
fn merge_alias_meta_for_docs(
    target_meta: Option<&Meta>,
    alias_meta: Option<&Meta>,
) -> Option<Meta> {
    match (target_meta, alias_meta) {
        (None, None) => None,
        (Some(t), None) => Some(t.clone()),
        (None, Some(a)) => Some(a.clone()),
        (Some(t), Some(a)) => match (&t.val, &a.val) {
            (Val::Map(target_map), Val::Map(alias_map)) => {
                let mut merged = (**target_map).clone();
                for (k, v) in alias_map.iter() {
                    merged.insert(k.clone(), v.clone());
                }
                Some(Meta {
                    val: Val::Map(Box::new(merged)),
                })
            }
            // Non-map meta on either side: alias wins wholesale.
            _ => Some(a.clone()),
        },
    }
}

/// Render the fully-qualified function name pointed at by an alias's
/// `Value::Ref`. We always emit a colon-prefixed namespace path so the
/// renderer doesn't have to special-case same-namespace vs cross-namespace
/// references — both forms are uniformly displayable as `::ns/fn`.
fn format_ref_target(ref_value: &Ref, current_namespace: &str) -> String {
    match ref_value {
        Ref::Ns(ns_ref) => {
            let fn_name = ns_ref.function_name.as_deref().unwrap_or("");
            format!("{}/{}", ns_ref.ns, fn_name)
        }
        Ref::Var(var_ref) => {
            format!("{}/{}", current_namespace, var_ref.var.sym.name())
        }
    }
}

/// Best-effort signature lookup for an alias target: searches every
/// namespace contained in the same parsed `Program`. Hits when the
/// target lives in the same file (common for package-internal
/// aliases). Misses silently when the target is in a separate file —
/// the renderer falls back on the `alias_of` link in that case.
fn signatures_from_ref(
    ref_value: &Ref,
    program: &crate::lang::ast::Program,
    current_namespace: &str,
    resolver: &TypeResolver,
) -> Vec<FunctionSignature> {
    let (target_ns, target_fn) = match ref_value {
        Ref::Ns(ns_ref) => match ns_ref.function_name.as_deref() {
            Some(fn_name) => (ns_ref.ns.to_string(), fn_name.to_string()),
            None => return Vec::new(),
        },
        Ref::Var(var_ref) => (
            current_namespace.to_string(),
            var_ref.var.sym.name().to_string(),
        ),
    };

    let target_namespace = match program
        .namespaces
        .iter()
        .find(|(p, _)| p.to_string() == target_ns)
    {
        Some((_, ns)) => ns,
        None => return Vec::new(),
    };

    let target_value = target_namespace
        .scope
        .vars
        .iter()
        .find(|(v, _)| v.sym.name() == target_fn)
        .map(|(_, v)| v);

    match target_value {
        Some(Value::Fn(fn_defs)) => fn_defs
            .iter()
            .map(|fn_def| FunctionSignature {
                params: fn_def
                    .args
                    .args
                    .iter()
                    .map(|arg| FunctionParam {
                        name: arg.var.sym.name().to_string(),
                        type_annotation: arg
                            .type_annotation
                            .as_ref()
                            .map(|t| clean_type_annotation(t, resolver)),
                        is_lazy: arg.lazy,
                        is_variadic: false,
                    })
                    .collect(),
                return_type: fn_def
                    .return_type
                    .as_ref()
                    .map(|t| clean_type_annotation(t, resolver)),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Check if metadata has core: true
fn is_core_from_meta(meta: &Option<Meta>) -> bool {
    meta.as_ref()
        .and_then(|m| m.val.get("core"))
        .map(|v| matches!(v, Val::Bool(true)))
        .unwrap_or(false)
}

/// Check if metadata has "no-doc": true (to exclude from documentation)
fn is_no_doc_from_meta(meta: &Option<Meta>) -> bool {
    meta.as_ref()
        .and_then(|m| m.val.get("no-doc"))
        .map(|v| matches!(v, Val::Bool(true)))
        .unwrap_or(false)
}

/// Extract ctx requirements from metadata
/// New format: {ctx: {"key": {required: true, default: value, secret: true}}}
fn extract_ctx_from_meta(meta: &Option<Meta>) -> Option<CtxRequirements> {
    meta.as_ref().and_then(|m| {
        m.val.get("ctx").and_then(|ctx_val| {
            if let Val::Map(ctx_map) = ctx_val {
                let mut req: Vec<String> = Vec::new();
                let mut opt: Vec<String> = Vec::new();

                // Parse new per-key object format
                for (key, config) in ctx_map.iter() {
                    let key_str: String = match key {
                        Val::Str(s) => (*s).to_string(),
                        _ => continue, // Skip non-string keys
                    };

                    // Parse config to determine if required or optional
                    let is_required = match config {
                        Val::Map(config_map) => {
                            // Check if 'required' is explicitly set
                            let required_explicit = config_map
                                .get(&Val::from("required"))
                                .and_then(|v| if let Val::Bool(b) = v { Some(*b) } else { None });

                            // Check if 'default' is set (implies optional)
                            let has_default = config_map.contains_key(&Val::from("default"));

                            match required_explicit {
                                Some(r) => r,
                                None => !has_default, // default implies not required
                            }
                        }
                        _ => true, // Empty or non-map config defaults to required
                    };

                    if is_required {
                        req.push(key_str);
                    } else {
                        opt.push(key_str);
                    }
                }

                // Only return Some if there are any requirements
                if req.is_empty() && opt.is_empty() {
                    None
                } else {
                    Some(CtxRequirements { req, opt })
                }
            } else {
                None
            }
        })
    })
}

/// Extract box (container) requirements from metadata
/// Format: meta { box: { min-size: "medium", network: true } }
fn extract_box_from_meta(meta: &Option<Meta>) -> Option<BoxRequirementDoc> {
    meta.as_ref().and_then(|m| {
        m.val.get("box").and_then(|box_val| {
            if let Val::Map(box_map) = box_val {
                let min_size = box_map.get(&Val::from("min-size")).and_then(|v| {
                    if let Val::Str(s) = v {
                        Some(s.to_string())
                    } else {
                        None
                    }
                });
                let network = box_map
                    .get(&Val::from("network"))
                    .and_then(|v| if let Val::Bool(b) = v { Some(*b) } else { None })
                    .unwrap_or(false);

                if min_size.is_some() || network {
                    Some(BoxRequirementDoc { min_size, network })
                } else {
                    None
                }
            } else {
                None
            }
        })
    })
}

/// Extract schedule cron expression from metadata
fn extract_schedule_from_meta(meta: &Option<Meta>) -> Option<String> {
    meta.as_ref().and_then(|m| match &m.val {
        Val::Map(meta_map) => {
            if let Some(Val::Str(cron)) = meta_map.get(&Val::from("schedule")) {
                Some((*cron).to_string())
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Extract event handler type from metadata
fn extract_on_event_from_meta(meta: &Option<Meta>) -> Option<String> {
    meta.as_ref().and_then(|m| match &m.val {
        Val::Map(meta_map) => {
            if let Some(Val::Str(event_type)) = meta_map.get(&Val::from("on-event")) {
                Some((*event_type).to_string())
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Extract webhook configuration from metadata
fn extract_webhook_from_meta(meta: &Option<Meta>) -> Option<DocWebhook> {
    meta.as_ref().and_then(|m| match &m.val {
        Val::Map(meta_map) => {
            if let Some(Val::Map(wh_map)) = meta_map.get(&Val::from("webhook")) {
                let service = wh_map
                    .get(&Val::from("service"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((*s).to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let path = wh_map
                    .get(&Val::from("path"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((*s).to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                if service.is_empty() {
                    None
                } else {
                    Some(DocWebhook { service, path })
                }
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Extract MCP tool configuration from metadata
fn extract_mcp_from_meta(meta: &Option<Meta>) -> Option<DocMcp> {
    meta.as_ref().and_then(|m| match &m.val {
        Val::Map(meta_map) => {
            if let Some(Val::Map(mcp_map)) = meta_map.get(&Val::from("mcp")) {
                let service = mcp_map
                    .get(&Val::from("service"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((*s).to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let name = mcp_map
                    .get(&Val::from("name"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((*s).to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                if service.is_empty() {
                    None
                } else {
                    Some(DocMcp { service, name })
                }
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Extract send event names from metadata (meta {sends: [...]})
fn extract_sends_from_meta(meta: &Option<Meta>) -> Vec<String> {
    let arr = match meta.as_ref().map(|m| &m.val) {
        Some(Val::Map(meta_map)) => match meta_map.get(&Val::from("sends")) {
            Some(Val::Vec(v)) => v,
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    arr.iter()
        .filter_map(|item| match item {
            Val::Str(s) => Some(s.to_string()),
            Val::Map(obj) => obj.get(&Val::from("event")).and_then(|v| {
                if let Val::Str(s) = v {
                    Some(s.to_string())
                } else {
                    None
                }
            }),
            _ => None,
        })
        .collect()
}

/// Resolution context for a single namespace: its declared namespace aliases
/// (e.g. `::store ::hot::store`) and its var imports that point at items in
/// other namespaces (e.g. `Session ::session/Session`). Both are used by
/// [`clean_type_annotation`] to produce fully-qualified types in the docs.
#[derive(Default)]
struct TypeResolver {
    /// Sorted alias prefix replacements, longest first.
    aliases: Vec<(String, String)>,
    /// Bare-identifier replacements, e.g. `"Session" -> "::ai::session/Session"`.
    var_imports: AHashMap<String, String>,
}

impl TypeResolver {
    fn from_namespace(ns: &crate::lang::ast::Namespace) -> Self {
        // Sort alias entries longest-first so `::a::b` wins over `::a`.
        let mut aliases: Vec<(String, String)> = ns
            .aliases
            .iter()
            .map(|(alias, real)| (alias.to_string(), real.to_string()))
            .collect();
        aliases.sort_by_key(|a| std::cmp::Reverse(a.0.len()));

        // Walk vars looking for namespace-ref imports (`Session ::session/Session`,
        // `sha256 ::hot::hash/sha256`, ...). The import target may itself use an
        // alias, so resolve that path through `aliases` before storing it.
        let mut var_imports = AHashMap::new();
        for (var, value) in &ns.scope.vars {
            if let Value::Ref(Ref::Ns(ns_ref)) = value
                && let Some(fn_name) = &ns_ref.function_name
            {
                let resolved_ns = resolve_ns_path(&ns_ref.ns, &aliases);
                var_imports.insert(
                    var.sym.name().to_string(),
                    format!("{}/{}", resolved_ns, fn_name),
                );
            }
        }

        Self {
            aliases,
            var_imports,
        }
    }
}

/// Clean up a type annotation string, expanding any namespace aliases or
/// var-style imports declared in the enclosing namespace into their
/// fully-qualified form.
///
/// For example, given an alias `::store ::hot::store` and a var import
/// `Session ::session/Session` (where `::session` is itself aliased to
/// `::ai::session`), `::store/Map` becomes `::hot::store/Map` and a bare
/// `Session` becomes `::ai::session/Session`. Built-in types (`Str`, `Int`,
/// ...) and locally-defined names that aren't imports are left untouched.
fn clean_type_annotation(s: &str, resolver: &TypeResolver) -> String {
    resolve_type_aliases(s, resolver)
}

/// Apply the namespace alias map to a single `NsPath`, returning its
/// fully-qualified string form. Used when expanding the target of a var import.
fn resolve_ns_path(path: &NsPath, aliases: &[(String, String)]) -> String {
    let s = path.to_string();
    for (alias, real) in aliases {
        if s == *alias {
            return real.clone();
        }
        if s.len() > alias.len() && s.starts_with(alias) && s[alias.len()..].starts_with("::") {
            let mut out = String::with_capacity(real.len() + s.len() - alias.len());
            out.push_str(real);
            out.push_str(&s[alias.len()..]);
            return out;
        }
    }
    s
}

/// Walk a type-annotation string and rewrite:
///   1. `::ident(::ident)*` runs whose prefix matches a known namespace alias,
///      and
///   2. bare identifiers that match a known var import.
///
/// The string may contain unions, generics, and optional markers (e.g.
/// `Vec<::store/Map>`, `::store/Map | Null`, `Session?`), so we tokenize it
/// rather than doing naive substring replacement.
fn resolve_type_aliases(s: &str, resolver: &TypeResolver) -> String {
    if resolver.aliases.is_empty() && resolver.var_imports.is_empty() {
        return s.to_string();
    }

    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    // True if the previous emitted byte ended an identifier-like token; used to
    // ensure we only treat a run of ident chars as a "bare identifier" when it
    // isn't a continuation of something else (like `::foo`).
    let mut prev_was_ident_continuation = false;

    while i < bytes.len() {
        let c = bytes[i];

        if c == b':' && i + 1 < bytes.len() && bytes[i + 1] == b':' {
            // Capture the full ::a::b::c run (identifier chars + nested ::).
            let mut end = i + 2;
            while end < bytes.len() {
                let nc = bytes[end];
                if nc.is_ascii_alphanumeric() || nc == b'_' || nc == b'-' {
                    end += 1;
                } else if nc == b':' && end + 1 < bytes.len() && bytes[end + 1] == b':' {
                    end += 2;
                } else {
                    break;
                }
            }
            let ns_run = &s[i..end];

            let mut replaced = false;
            for (alias, real) in &resolver.aliases {
                if ns_run == alias.as_str()
                    || (ns_run.len() > alias.len()
                        && ns_run.starts_with(alias.as_str())
                        && ns_run[alias.len()..].starts_with("::"))
                {
                    out.push_str(real);
                    out.push_str(&ns_run[alias.len()..]);
                    replaced = true;
                    break;
                }
            }
            if !replaced {
                out.push_str(ns_run);
            }
            i = end;
            prev_was_ident_continuation = true;
        } else if !prev_was_ident_continuation && (c.is_ascii_alphabetic() || c == b'_') {
            // Start of a bare identifier (not preceded by `::` or another ident
            // char). Capture the full identifier and try to resolve it.
            let start = i;
            i += 1;
            while i < bytes.len() {
                let nc = bytes[i];
                if nc.is_ascii_alphanumeric() || nc == b'_' || nc == b'-' {
                    i += 1;
                } else {
                    break;
                }
            }
            let ident = &s[start..i];
            if let Some(replacement) = resolver.var_imports.get(ident) {
                out.push_str(replacement);
            } else {
                out.push_str(ident);
            }
            prev_was_ident_continuation = true;
        } else {
            // Non-identifier byte (whitespace, |, <, >, ?, ,, (, ), etc.).
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            prev_was_ident_continuation = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-';
        }
    }
    out
}

enum ParsedItem {
    // Boxed to keep the enum compact: `DocFunction` carries a number
    // of optional/vec metadata fields and dwarfs `DocType` in size.
    Function(Box<DocFunction>),
    Type(DocType),
}

/// Try to parse a function or type starting at line index
fn try_parse_item(lines: &[&str], start: usize) -> Option<(ParsedItem, usize)> {
    let line = lines[start].trim();

    // Skip if starts with special characters or keywords we don't handle
    if line.is_empty()
        || line.starts_with("//")
        || line.starts_with("$")
        || line.starts_with('{')
        || line.starts_with('}')
        || line.starts_with('(')
        || line.starts_with(')')
    {
        return None;
    }

    // Extract the identifier name (first token)
    let mut name = String::new();
    let mut chars = line.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == '?' || ch == '!' {
            name.push(ch);
            chars.next();
        } else {
            break;
        }
    }

    if name.is_empty() {
        return None;
    }

    let rest = line[name.len()..].trim();

    // Check if this has metadata or is a function/type definition
    let has_metadata = rest.starts_with('#');
    let is_function = rest.contains("fn") || rest.contains("fn ");
    let is_type = rest.starts_with("type") || rest.contains(" type");

    if !has_metadata && !is_function && !is_type {
        return None;
    }

    // Parse metadata if present
    let (doc, is_core, meta_end) = if has_metadata {
        parse_metadata(lines, start)
    } else {
        (None, false, start)
    };

    // Determine what kind of definition this is
    // Look at lines from start to meta_end to find "fn" or "type"
    // NOTE: Check for "type" FIRST because types can have constructors like:
    //   "Name type {fields} fn" - this is a TYPE, not a function
    let mut definition_type = None;
    for check_line in lines
        .iter()
        .take(meta_end.min(lines.len() - 1) + 1)
        .skip(start)
    {
        // Check for type first (types may also contain "fn" for constructors)
        if check_line.contains(" type") || check_line.contains("\ttype") {
            definition_type = Some("type");
            break;
        }
        if check_line.contains(" fn")
            || check_line.contains("\tfn")
            || check_line.trim().starts_with("fn")
        {
            definition_type = Some("fn");
            break;
        }
    }

    // Also check the line after metadata
    if definition_type.is_none() && meta_end + 1 < lines.len() {
        let next_line = lines[meta_end + 1].trim();
        if next_line.starts_with("fn") {
            definition_type = Some("fn");
        } else if next_line.starts_with("type") {
            definition_type = Some("type");
        }
    }

    let definition_type = definition_type?;

    // Find the end of the definition (matching braces)
    let end_line = find_definition_end(lines, start);

    // Parse signatures for functions
    let signatures = if definition_type == "fn" {
        parse_function_signatures(lines, start, end_line)
    } else {
        Vec::new()
    };

    // Parse fields for types
    let fields = if definition_type == "type" {
        parse_type_fields(lines, start, end_line)
    } else {
        Vec::new()
    };

    match definition_type {
        "fn" => Some((
            ParsedItem::Function(Box::new(DocFunction {
                name,
                doc,
                is_core,
                signatures,
                ctx: None,      // Text parser doesn't support ctx; use AST parser
                box_req: None,  // Text parser doesn't support box; use AST parser
                schedule: None, // Text parser doesn't extract meta indicators
                on_event: None,
                webhook: None,
                mcp: None,
                sends: Vec::new(),
                alias_of: None,
            })),
            end_line,
        )),
        "type" => Some((
            ParsedItem::Type(DocType {
                name,
                doc,
                is_core,
                fields,
                constructors: Vec::new(), // Text parser doesn't extract constructors
                type_alias: None,         // Text parser doesn't extract type aliases
            }),
            end_line,
        )),
        _ => None,
    }
}

/// Parse metadata block and extract doc and core
fn parse_metadata(lines: &[&str], start: usize) -> (Option<String>, bool, usize) {
    let mut doc = None;
    let mut is_core = false;
    let mut end_line = start;

    // Collect metadata content (may span multiple lines)
    let mut meta_content = String::new();
    let mut brace_count = 0;
    let mut bracket_count = 0;
    let mut in_meta = false;
    let mut found_meta_end = false;

    for (i, line) in lines.iter().enumerate().skip(start) {
        for ch in line.chars() {
            if ch == '#' && !in_meta {
                in_meta = true;
            }
            if in_meta {
                meta_content.push(ch);
                if ch == '{' {
                    brace_count += 1;
                } else if ch == '}' {
                    brace_count -= 1;
                    if brace_count == 0 && bracket_count == 0 {
                        found_meta_end = true;
                    }
                } else if ch == '[' {
                    bracket_count += 1;
                } else if ch == ']' {
                    bracket_count -= 1;
                    if brace_count == 0 && bracket_count == 0 {
                        found_meta_end = true;
                    }
                }
            }
        }
        meta_content.push('\n');
        end_line = i;

        if found_meta_end {
            break;
        }
    }

    // Parse doc from metadata
    if let Some(doc_start) = meta_content.find("\"doc\"")
        && let Some(colon) = meta_content[doc_start..].find(':')
    {
        let after_colon = &meta_content[doc_start + colon + 1..];
        if let Some(quote_start) = after_colon.find('"') {
            let doc_text = &after_colon[quote_start + 1..];
            // Find closing quote that's not escaped
            if let Some(quote_end) = find_unescaped_quote(doc_text) {
                // Process escape sequences in doc string
                doc = Some(unescape_string(&doc_text[..quote_end]).trim().to_string());
            }
        }
    }

    // Parse core from metadata
    if meta_content.contains("\"core\"") && meta_content.contains("true") {
        is_core = true;
    }
    if meta_content.contains("#[\"core\"]") || meta_content.contains("#[ \"core\" ]") {
        is_core = true;
    }

    (doc, is_core, end_line)
}

/// Find the end of a definition by matching braces
fn find_definition_end(lines: &[&str], start: usize) -> usize {
    let mut brace_count = 0;
    let mut found_brace = false;

    for (i, line) in lines.iter().enumerate().skip(start) {
        for ch in line.chars() {
            if ch == '{' {
                brace_count += 1;
                found_brace = true;
            } else if ch == '}' {
                brace_count -= 1;
            }
        }
        if found_brace && brace_count == 0 {
            return i;
        }
    }

    lines.len().saturating_sub(1)
}

/// Parse function signatures from definition lines
fn parse_function_signatures(lines: &[&str], start: usize, end: usize) -> Vec<FunctionSignature> {
    let mut signatures = Vec::new();
    let mut current_params = Vec::new();
    let mut current_return = None;
    let mut in_params = false;
    let mut paren_depth = 0;
    let mut current_param = String::new();

    // Combine relevant lines
    let combined: String = lines[start..=end].join("\n");

    // Find parameter lists: (param: Type, param2: Type): ReturnType
    let mut chars = combined.chars().peekable();
    let mut after_fn = false;

    while let Some(ch) = chars.next() {
        // Look for "fn" keyword
        if ch == 'f' && chars.peek() == Some(&'n') {
            chars.next();
            after_fn = true;
            continue;
        }

        if !after_fn {
            continue;
        }

        match ch {
            '(' => {
                paren_depth += 1;
                if paren_depth == 1 {
                    in_params = true;
                    current_param.clear();
                } else if in_params {
                    current_param.push(ch);
                }
            }
            ')' => {
                paren_depth -= 1;
                if paren_depth == 0 && in_params {
                    // End of params
                    if !current_param.trim().is_empty()
                        && let Some(param) = parse_param(&current_param)
                    {
                        current_params.push(param);
                    }
                    in_params = false;

                    // Look for return type
                    let remaining: String = chars.clone().collect();
                    if let Some(ret) = parse_return_type(&remaining) {
                        current_return = Some(ret);
                    }

                    // Save signature
                    signatures.push(FunctionSignature {
                        params: current_params.clone(),
                        return_type: current_return.clone(),
                    });

                    current_params.clear();
                    current_return = None;
                } else if in_params {
                    current_param.push(ch);
                }
            }
            ',' if in_params && paren_depth == 1 => {
                if !current_param.trim().is_empty()
                    && let Some(param) = parse_param(&current_param)
                {
                    current_params.push(param);
                }
                current_param.clear();
            }
            _ if in_params => {
                current_param.push(ch);
            }
            _ => {}
        }
    }

    // If no signatures found, add a default one
    if signatures.is_empty() {
        signatures.push(FunctionSignature {
            params: Vec::new(),
            return_type: None,
        });
    }

    signatures
}

/// Parse a single parameter string like "name: Type" or "lazy name: Type"
fn parse_param(s: &str) -> Option<FunctionParam> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let is_lazy = s.starts_with("lazy ");
    let is_variadic = s.starts_with("...");

    let s = s.strip_prefix("lazy ").unwrap_or(s);
    let s = s.strip_prefix("...").unwrap_or(s);

    let (name, type_annotation) = if let Some(colon_pos) = s.find(':') {
        let name = s[..colon_pos].trim().to_string();
        let typ = s[colon_pos + 1..].trim().to_string();
        (name, if typ.is_empty() { None } else { Some(typ) })
    } else {
        (s.trim().to_string(), None)
    };

    Some(FunctionParam {
        name,
        type_annotation,
        is_lazy,
        is_variadic,
    })
}

/// Parse return type from string like ": ReturnType {"
fn parse_return_type(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.starts_with(':') {
        return None;
    }

    let s = s[1..].trim();

    // Find where the return type ends (before { or end of line)
    let end = s.find('{').unwrap_or(s.len());
    let ret = s[..end].trim();

    if ret.is_empty() {
        None
    } else {
        Some(ret.to_string())
    }
}

/// Parse type fields from definition
fn parse_type_fields(lines: &[&str], start: usize, end: usize) -> Vec<TypeField> {
    let mut fields = Vec::new();

    // Combine lines from start to end
    let combined: String = lines[start..=end].join("\n");

    // Find the type keyword
    if let Some(type_pos) = combined.find("type") {
        let after_type = &combined[type_pos + 4..];

        // Check what comes after "type" - could be:
        // 1. "type { fields }" - struct with fields
        // 2. "type fn (...)" - type with only constructor
        // 3. "type { fields } fn (...)" - struct with fields AND constructor

        let trimmed = after_type.trim_start();

        // If it starts with "fn", there are no fields
        if trimmed.starts_with("fn") {
            return fields;
        }

        // If it doesn't start with "{", there are no fields (e.g., empty marker type)
        if !trimmed.starts_with('{') {
            return fields;
        }

        // Find the matching closing brace for the type body
        let after_brace = &trimmed[1..]; // skip the opening brace
        let mut depth = 1;
        let mut brace_end = 0;
        for (i, ch) in after_brace.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        brace_end = i;
                        break;
                    }
                }
                _ => {}
            }
        }

        if brace_end > 0 {
            let body = &after_brace[..brace_end];

            // Parse field definitions: "name: Type," or "name: Type?"
            for field_str in body.split(',') {
                let field_str = field_str.trim();
                if field_str.is_empty() {
                    continue;
                }

                // Skip if this contains function-like syntax
                if field_str.contains('(')
                    || field_str.contains(')')
                    || field_str.contains('{')
                    || field_str.contains('}')
                    || field_str.contains("call-lib")
                    || field_str.contains("fn")
                {
                    continue;
                }

                // Parse "name: Type"
                if let Some(colon_pos) = field_str.find(':') {
                    let name = field_str[..colon_pos].trim();
                    let type_str = field_str[colon_pos + 1..].trim();

                    // Clean up the name (remove newlines, extra whitespace)
                    let name = name.split_whitespace().last().unwrap_or(name);

                    // Skip if name is empty or doesn't look like an identifier
                    if name.is_empty()
                        || !name
                            .chars()
                            .next()
                            .map(|c| c.is_alphabetic() || c == '_' || c == '$')
                            .unwrap_or(false)
                    {
                        continue;
                    }

                    fields.push(TypeField {
                        name: name.to_string(),
                        type_annotation: if type_str.is_empty() {
                            None
                        } else {
                            Some(type_str.to_string())
                        },
                    });
                }
            }
        }
    }

    fields
}

/// Generate JSON documentation for a package and save it to a file (legacy, unversioned)
pub fn generate_pkg_docs_json(pkg_name: &str, output_path: &Path) -> Result<(), String> {
    let pkg_path =
        find_pkg_path(pkg_name).ok_or_else(|| format!("Package '{}' not found", pkg_name))?;

    let pkg_docs = load_pkg_docs(&pkg_path)?;

    let json = serde_json::to_string_pretty(&pkg_docs)
        .map_err(|e| format!("Failed to serialize docs: {}", e))?;

    // Ensure parent directory exists
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    fs::write(output_path, json).map_err(|e| format!("Failed to write docs file: {}", e))?;

    tracing::info!("Generated docs JSON for {} at {:?}", pkg_name, output_path);
    Ok(())
}

/// Generate versioned JSON documentation for a package
///
/// Creates the following structure:
/// - `{out_dir}/{pkg_name}/versions.json` - index of available versions
/// - `{out_dir}/{pkg_name}/{version}/docs.json` - docs for specific version
///
/// If `served_packages` is provided, deps that match a served package will get
/// a `docs_url` field pointing to the local docs (e.g., "/pkg/aws-core").
pub fn generate_versioned_pkg_docs(pkg_name: &str, out_dir: &Path) -> Result<String, String> {
    generate_versioned_pkg_docs_with_context(pkg_name, out_dir, &[])
}

/// Generate versioned JSON documentation with context about served packages
///
/// Same as `generate_versioned_pkg_docs` but allows specifying which packages
/// are being served so deps can link to their local docs.
pub fn generate_versioned_pkg_docs_with_context(
    pkg_name: &str,
    out_dir: &Path,
    served_packages: &[&str],
) -> Result<String, String> {
    let pkg_path =
        find_pkg_path(pkg_name).ok_or_else(|| format!("Package '{}' not found", pkg_name))?;

    let mut pkg_docs = load_pkg_docs(&pkg_path)?;
    let version = pkg_docs.meta.version.clone();

    if version.is_empty() {
        return Err(format!(
            "Package '{}' has no version defined in pkg.hot",
            pkg_name
        ));
    }

    // Populate docs_url for deps that are served locally
    for dep in &mut pkg_docs.meta.deps {
        if served_packages.contains(&dep.name.as_str()) {
            dep.docs_url = Some(format!("/pkg/{}", dep.name));
        }
    }

    // Create versioned output directory: {out_dir}/{pkg_name}/{version}/
    let pkg_dir = out_dir.join(pkg_name);
    let version_dir = pkg_dir.join(&version);
    fs::create_dir_all(&version_dir).map_err(|e| format!("Failed to create directory: {}", e))?;

    // Write docs.json for this version
    let docs_path = version_dir.join("docs.json");
    let json = serde_json::to_string_pretty(&pkg_docs)
        .map_err(|e| format!("Failed to serialize docs: {}", e))?;
    fs::write(&docs_path, json).map_err(|e| format!("Failed to write docs file: {}", e))?;

    // Update versions.json
    let versions_path = pkg_dir.join("versions.json");
    let mut versions_index =
        load_versions_index(&versions_path).unwrap_or_else(|_| PkgVersionsIndex {
            latest: version.clone(),
            versions: vec![],
        });

    // Add version if not already present
    if !versions_index.versions.contains(&version) {
        versions_index.versions.insert(0, version.clone());
    }

    // Update latest to current version
    versions_index.latest = version.clone();

    // Sort versions (newest first, using semver-like comparison)
    versions_index.versions.sort_by(|a, b| {
        compare_versions(b, a) // Reverse order for newest first
    });

    let versions_json = serde_json::to_string_pretty(&versions_index)
        .map_err(|e| format!("Failed to serialize versions: {}", e))?;
    fs::write(&versions_path, versions_json)
        .map_err(|e| format!("Failed to write versions file: {}", e))?;

    tracing::info!(
        "Generated versioned docs for {}@{} at {:?}",
        pkg_name,
        version,
        version_dir
    );
    Ok(version)
}

/// Load versions index from file
pub fn load_versions_index(path: &Path) -> Result<PkgVersionsIndex, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("Failed to read versions file: {}", e))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse versions JSON: {}", e))
}

/// Compare version strings (simple semver-like comparison)
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse_version =
        |s: &str| -> Vec<u32> { s.split('.').filter_map(|part| part.parse().ok()).collect() };

    let va = parse_version(a);
    let vb = parse_version(b);

    for (pa, pb) in va.iter().zip(vb.iter()) {
        match pa.cmp(pb) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }

    va.len().cmp(&vb.len())
}

/// Load package documentation from a pre-generated JSON file
pub fn load_pkg_docs_from_json(json_path: &Path) -> Result<PkgDocs, String> {
    let content = fs::read_to_string(json_path)
        .map_err(|e| format!("Failed to read docs file {:?}: {}", json_path, e))?;

    serde_json::from_str(&content).map_err(|e| format!("Failed to parse docs JSON: {}", e))
}

/// Load versioned package documentation
///
/// If version is None, loads the latest version.
/// Returns (PkgDocs, actual_version, versions_index)
pub fn load_versioned_pkg_docs(
    pkg_name: &str,
    version: Option<&str>,
    docs_base_dir: &Path,
) -> Result<(PkgDocs, String, PkgVersionsIndex), String> {
    let pkg_dir = docs_base_dir.join(pkg_name);
    let versions_path = pkg_dir.join("versions.json");

    // Load versions index
    let versions_index = load_versions_index(&versions_path)?;

    // Determine which version to load
    let target_version = version.unwrap_or(&versions_index.latest);

    // Validate version exists
    if !versions_index
        .versions
        .contains(&target_version.to_string())
    {
        return Err(format!(
            "Version '{}' not found for package '{}'. Available: {:?}",
            target_version, pkg_name, versions_index.versions
        ));
    }

    // Load docs for target version
    let docs_path = pkg_dir.join(target_version).join("docs.json");
    let pkg_docs = load_pkg_docs_from_json(&docs_path)?;

    Ok((pkg_docs, target_version.to_string(), versions_index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_value() {
        assert_eq!(
            parse_key_value(r#""name": "hot-std","#),
            Some(("name".to_string(), "hot-std".to_string()))
        );
        assert_eq!(
            parse_key_value(r#""version": "0.1.0""#),
            Some(("version".to_string(), "0.1.0".to_string()))
        );
    }

    #[test]
    fn test_find_hot_std() {
        // This test will only pass in development environment
        if let Some(path) = find_hot_std_path() {
            assert!(path.exists());
            assert!(path.join("pkg.hot").exists());
        }
    }

    fn make_resolver(alias_pairs: &[(&str, &str)], var_pairs: &[(&str, &str)]) -> TypeResolver {
        let mut aliases: Vec<(String, String)> = alias_pairs
            .iter()
            .map(|(a, r)| (format!("::{}", a), format!("::{}", r)))
            .collect();
        aliases.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
        let var_imports = var_pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        TypeResolver {
            aliases,
            var_imports,
        }
    }

    #[test]
    fn test_resolve_type_aliases_namespace() {
        let resolver = make_resolver(&[("store", "hot::store"), ("session", "ai::session")], &[]);

        assert_eq!(
            resolve_type_aliases("::store/Map", &resolver),
            "::hot::store/Map"
        );
        assert_eq!(
            resolve_type_aliases("Vec<::store/Map>", &resolver),
            "Vec<::hot::store/Map>"
        );
        assert_eq!(
            resolve_type_aliases("::store/Map | Null", &resolver),
            "::hot::store/Map | Null"
        );
        assert_eq!(
            resolve_type_aliases("::store/Map?", &resolver),
            "::hot::store/Map?"
        );
        assert_eq!(
            resolve_type_aliases("::hot::store/Map", &resolver),
            "::hot::store/Map"
        );
        assert_eq!(resolve_type_aliases("Str", &resolver), "Str");
        assert_eq!(
            resolve_type_aliases("::unknown/Foo", &resolver),
            "::unknown/Foo"
        );

        let empty = TypeResolver::default();
        assert_eq!(resolve_type_aliases("::store/Map", &empty), "::store/Map");

        // Longest-prefix wins when one alias is a prefix of another
        let nested = make_resolver(&[("a", "x"), ("a::b", "y")], &[]);
        assert_eq!(resolve_type_aliases("::a::b/T", &nested), "::y/T");
        assert_eq!(resolve_type_aliases("::a/T", &nested), "::x/T");
    }

    #[test]
    fn test_resolve_type_aliases_var_imports() {
        // `Session ::session/Session` where `::session` itself aliases to
        // `::ai::session` -> bare `Session` should resolve all the way through
        // to `::ai::session/Session`.
        let resolver = make_resolver(
            &[("session", "ai::session")],
            &[("Session", "::ai::session/Session")],
        );

        assert_eq!(
            resolve_type_aliases("Session", &resolver),
            "::ai::session/Session"
        );
        assert_eq!(
            resolve_type_aliases("Session?", &resolver),
            "::ai::session/Session?"
        );
        assert_eq!(
            resolve_type_aliases("Vec<Session>", &resolver),
            "Vec<::ai::session/Session>"
        );
        assert_eq!(
            resolve_type_aliases("Session | Null", &resolver),
            "::ai::session/Session | Null"
        );

        // Built-ins and unknown bare identifiers are left untouched.
        assert_eq!(resolve_type_aliases("Str", &resolver), "Str");
        assert_eq!(
            resolve_type_aliases("AgentMemory", &resolver),
            "AgentMemory"
        );

        // Bare identifier inside a parameter signature shouldn't get matched
        // against the alias prefix portion.
        assert_eq!(
            resolve_type_aliases("(s: Session): Bool", &resolver),
            "(s: ::ai::session/Session): Bool"
        );
    }

    #[test]
    fn test_resolve_ns_path_via_alias() {
        let aliases = vec![("::session".to_string(), "::ai::session".to_string())];
        assert_eq!(
            resolve_ns_path(&NsPath::from_string("session"), &aliases),
            "::ai::session"
        );
        assert_eq!(
            resolve_ns_path(&NsPath::from_string("session::sub"), &aliases),
            "::ai::session::sub"
        );
        assert_eq!(
            resolve_ns_path(&NsPath::from_string("hot::http"), &aliases),
            "::hot::http"
        );
    }

    /// Helper: write `source` to a temp file and run the AST-based
    /// namespace parser on it. Returns the resulting `DocNamespace`.
    fn parse_doc_namespace(source: &str) -> DocNamespace {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ns.hot");
        let mut file = fs::File::create(&path).expect("create");
        file.write_all(source.as_bytes()).expect("write");
        parse_namespace_from_ast(&path).expect("parse namespace")
    }

    #[test]
    fn plain_var_alias_appears_in_docs_with_alias_of_link() {
        // No `meta` on the alias — just `re-exported ::lib::pkg/utility`.
        // This is the general var-alias case (re-export / shorthand)
        // and should produce a `DocFunction` with `alias_of` set.
        // Note: alias-bearing namespace comes first because
        // `parse_namespace_from_ast` returns the first one found.
        let source = r#"
::user::wrapper ns

re-exported ::lib::pkg/utility

::lib::pkg ns
utility meta { doc: "library utility" } fn (n: Int): Int { n }
"#;
        let ns = parse_doc_namespace(source);
        let func = ns
            .functions
            .iter()
            .find(|f| f.name == "re-exported")
            .expect("plain alias should appear in docs");
        assert_eq!(func.alias_of.as_deref(), Some("::lib::pkg/utility"));
        // Plain re-export inherits the target's doc string so users
        // see what the function does without bouncing.
        assert_eq!(func.doc.as_deref(), Some("library utility"));
        // And the target's signature.
        assert_eq!(func.signatures.len(), 1);
        assert_eq!(func.signatures[0].params.len(), 1);
        assert_eq!(func.signatures[0].params[0].name, "n");
    }

    #[test]
    fn meta_bearing_alias_merges_doc_from_target_with_own_overrides() {
        // Alias supplies `on-event`; target supplies `doc`. Merged
        // result should have both, with alias winning on collisions.
        let source = r#"
::user::agent ns

tg-handler
meta { on-event: "tg:msg" }
::lib::pkg/handler

::lib::pkg ns
handler meta { doc: "library handler" } fn (event: Map): Map { event }
"#;
        let ns = parse_doc_namespace(source);
        let func = ns
            .functions
            .iter()
            .find(|f| f.name == "tg-handler")
            .expect("wrapper alias should appear in docs");
        assert_eq!(func.alias_of.as_deref(), Some("::lib::pkg/handler"));
        // Inherited from target.
        assert_eq!(func.doc.as_deref(), Some("library handler"));
        // Declared by alias.
        assert_eq!(func.on_event.as_deref(), Some("tg:msg"));
    }

    #[test]
    fn alias_meta_overrides_target_meta_in_docs() {
        // Both alias and target declare `doc`; alias wins.
        let source = r#"
::user::ns ns

re-doc meta { doc: "wrapper doc" } ::lib::pkg/worker

::lib::pkg ns
worker meta { doc: "library doc" } fn (): Int { 1 }
"#;
        let ns = parse_doc_namespace(source);
        let func = ns
            .functions
            .iter()
            .find(|f| f.name == "re-doc")
            .expect("alias should appear in docs");
        assert_eq!(func.doc.as_deref(), Some("wrapper doc"));
    }
}

// =============================================================================
// Project Documentation (for user's source code, not packages)
// =============================================================================

/// Documentation for a project (user's src_paths, not a package)
///
/// Similar to PkgDocs but simpler - no pkg.hot metadata required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDocs {
    /// Project name
    pub name: String,
    /// Namespaces discovered from src_paths
    pub namespaces: Vec<DocNamespace>,
    /// Type index for cross-namespace linking
    #[serde(default)]
    pub type_index: AHashMap<String, String>,
}

/// Build documentation bundle - project docs + dependency docs
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildDocs {
    /// Documentation for the project's own code
    pub project: Option<ProjectDocs>,
    /// Documentation for each dependency (keyed by package name)
    pub deps: AHashMap<String, PkgDocs>,
}

impl BuildDocs {
    /// Create an empty BuildDocs
    pub fn new() -> Self {
        Self::default()
    }

    /// Serialize to JSON string
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize build docs: {}", e))
    }

    /// Deserialize from JSON string
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("Failed to parse build docs: {}", e))
    }
}

/// Generate documentation for a project from its source paths
///
/// This parses all .hot files in the given src_paths and extracts
/// namespace documentation, similar to how package docs are generated.
pub fn generate_project_docs(
    project_name: &str,
    src_paths: &[String],
) -> Result<ProjectDocs, String> {
    let mut all_namespaces = Vec::new();

    for src_path in src_paths {
        let path = Path::new(src_path);
        if path.exists() {
            let namespaces = discover_namespaces(path)?;
            all_namespaces.extend(namespaces);
        }
    }

    // Build type index
    let type_index = build_type_index(&all_namespaces);

    Ok(ProjectDocs {
        name: project_name.to_string(),
        namespaces: all_namespaces,
        type_index,
    })
}

/// Generate documentation for all resolved dependencies
///
/// For each dependency:
/// - If it has a local path with source, parse dynamically
/// - If it has cached docs.json, load from cache
/// - Otherwise, try to parse from the resolved path
pub fn generate_dependency_docs(
    resolved_deps: &[crate::lang::project::ResolvedDependency],
) -> AHashMap<String, PkgDocs> {
    let mut dep_docs = AHashMap::new();

    for dep in resolved_deps {
        // Skip hot-std (it's always available and handled separately)
        if dep.name == "hot-std" {
            continue;
        }

        tracing::debug!("Generating docs for dependency: {}", dep.name);

        // Try to load docs from the resolved path
        match load_pkg_docs(&dep.resolved_path) {
            Ok(docs) => {
                dep_docs.insert(dep.name.clone(), docs);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to generate docs for dependency '{}': {}",
                    dep.name,
                    e
                );
            }
        }
    }

    dep_docs
}

/// Generate complete build documentation (project + dependencies)
pub fn generate_build_docs(
    project_name: &str,
    src_paths: &[String],
    resolved_deps: &[crate::lang::project::ResolvedDependency],
) -> Result<BuildDocs, String> {
    // Generate project docs
    let project = match generate_project_docs(project_name, src_paths) {
        Ok(docs) => Some(docs),
        Err(e) => {
            tracing::warn!("Failed to generate project docs: {}", e);
            None
        }
    };

    // Generate dependency docs
    let deps = generate_dependency_docs(resolved_deps);

    Ok(BuildDocs { project, deps })
}
