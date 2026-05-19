use hot::db::{Build, DatabasePool, Env, Project};
use hot::val::Val;
use regex::RegexBuilder;
use serde::Serialize;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const SOURCE_PREFIXES: [&str; 2] = ["hot/src/", "hot/pkg/"];
const RESOURCE_PREFIX: &str = "resources/";
const MAX_BROWSABLE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_QUERY_BYTES: usize = 200;
const MAX_SEARCH_LINE_BYTES: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub struct SourceFileEntry {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub source_kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceTreeResponse {
    pub build_id: Uuid,
    pub build_type: String,
    pub files: Vec<SourceFileEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceFileResponse {
    pub build_id: Uuid,
    pub build_type: String,
    pub path: String,
    pub display_path: String,
    pub content: String,
    pub line: Option<usize>,
    pub language: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSearchResponse {
    pub build_id: Uuid,
    pub build_type: String,
    pub query: String,
    pub case_sensitive: bool,
    pub regex: bool,
    pub truncated: bool,
    pub results: Vec<SourceSearchMatch>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSearchMatch {
    pub path: String,
    pub display_path: String,
    pub line: usize,
    pub line_text: String,
    pub match_start: usize,
    pub match_end: usize,
}

#[derive(Debug, Clone)]
struct LocalSourceEntry {
    public: SourceFileEntry,
    abs_path: PathBuf,
}

pub async fn list_source_files(
    db: &DatabasePool,
    conf: &Val,
    build: &Build,
) -> Result<SourceTreeResponse, String> {
    if build.is_live() {
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| format!("failed to load project: {e}"))?;
        let files = discover_live_browsable_files(conf, &project)?
            .into_iter()
            .map(|entry| entry.public)
            .collect();

        return Ok(SourceTreeResponse {
            build_id: build.build_id,
            build_type: build.build_type.clone(),
            files,
        });
    }

    let build_data = retrieve_build_data(db, conf, build).await?;
    let files = list_bundle_source_files(&build_data)?;
    Ok(SourceTreeResponse {
        build_id: build.build_id,
        build_type: build.build_type.clone(),
        files,
    })
}

pub async fn read_source_file(
    db: &DatabasePool,
    conf: &Val,
    build: &Build,
    requested_path: &str,
    line: Option<usize>,
) -> Result<SourceFileResponse, String> {
    if build.is_live() {
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| format!("failed to load project: {e}"))?;
        let files = discover_live_browsable_files(conf, &project)?;
        let entry = resolve_live_source_entry(&files, requested_path)
            .ok_or_else(|| "source file not found".to_string())?;
        let content = tokio::fs::read_to_string(&entry.abs_path)
            .await
            .map_err(|e| format!("failed to read source file: {e}"))?;

        return Ok(SourceFileResponse {
            build_id: build.build_id,
            build_type: build.build_type.clone(),
            path: entry.public.path.clone(),
            display_path: entry.public.path.clone(),
            content,
            line,
            language: language_for_path(&entry.public.path),
        });
    }

    let build_data = retrieve_build_data(db, conf, build).await?;
    let resolved_path = resolve_bundle_source_path(&build_data, requested_path)?
        .ok_or_else(|| "source file not found".to_string())?;
    let content = read_bundle_source_content(&build_data, &resolved_path)?;

    Ok(SourceFileResponse {
        build_id: build.build_id,
        build_type: build.build_type.clone(),
        path: resolved_path.clone(),
        display_path: resolved_path.clone(),
        content,
        line,
        language: language_for_path(&resolved_path),
    })
}

pub async fn search_source_files(
    db: &DatabasePool,
    conf: &Val,
    build: &Build,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
) -> Result<SourceSearchResponse, String> {
    let matcher = SourceSearchMatcher::new(query, case_sensitive, is_regex)?;
    let mut results = Vec::new();
    let mut truncated = false;

    if build.is_live() {
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| format!("failed to load project: {e}"))?;
        let files = discover_live_browsable_files(conf, &project)?;
        for entry in files {
            let content = tokio::fs::read_to_string(&entry.abs_path)
                .await
                .map_err(|e| format!("failed to read source file: {e}"))?;
            truncated |= search_source_content(
                &entry.public.path,
                &content,
                &matcher,
                &mut results,
                MAX_SEARCH_RESULTS,
            );
            if results.len() >= MAX_SEARCH_RESULTS {
                break;
            }
        }
    } else {
        let build_data = retrieve_build_data(db, conf, build).await?;
        let cursor = Cursor::new(build_data);
        let mut archive =
            zip::ZipArchive::new(cursor).map_err(|e| format!("failed to read build zip: {e}"))?;

        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| format!("failed to read zip entry: {e}"))?;
            let path = file.name().to_string();
            if !file.is_file() || !is_browsable_entry(&path, file.size()) {
                continue;
            }

            let mut content = String::new();
            file.read_to_string(&mut content)
                .map_err(|e| format!("failed to read source as UTF-8: {e}"))?;
            drop(file);

            truncated |=
                search_source_content(&path, &content, &matcher, &mut results, MAX_SEARCH_RESULTS);
            if results.len() >= MAX_SEARCH_RESULTS {
                break;
            }
        }
    }

    Ok(SourceSearchResponse {
        build_id: build.build_id,
        build_type: build.build_type.clone(),
        query: query.to_string(),
        case_sensitive,
        regex: is_regex,
        truncated,
        results,
    })
}

async fn retrieve_build_data(
    db: &DatabasePool,
    conf: &Val,
    build: &Build,
) -> Result<Vec<u8>, String> {
    let project = Project::get_project(db, &build.project_id)
        .await
        .map_err(|e| format!("failed to load project: {e}"))?;
    let env = Env::get_env(db, &project.env_id)
        .await
        .map_err(|e| format!("failed to load environment: {e}"))?;
    let storage = hot::storage::build_storage_from_config(conf)
        .await
        .map_err(|e| format!("failed to initialize build storage: {e}"))?;
    storage
        .retrieve_build(&build.build_id, &env.org_id, &project.env_id)
        .await
        .map_err(|e| format!("failed to retrieve build: {e}"))
}

struct SourceSearchMatcher {
    regex: regex::Regex,
}

impl SourceSearchMatcher {
    fn new(query: &str, case_sensitive: bool, is_regex: bool) -> Result<Self, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("search query is required".to_string());
        }
        if query.len() > MAX_SEARCH_QUERY_BYTES {
            return Err(format!(
                "search query must be {MAX_SEARCH_QUERY_BYTES} bytes or less"
            ));
        }

        let pattern = if is_regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| format!("invalid regular expression: {e}"))?;

        Ok(Self { regex })
    }

    fn find<'a>(&'a self, line: &'a str) -> impl Iterator<Item = regex::Match<'a>> + 'a {
        self.regex.find_iter(line).filter(|m| !m.is_empty())
    }
}

fn search_source_content(
    path: &str,
    content: &str,
    matcher: &SourceSearchMatcher,
    results: &mut Vec<SourceSearchMatch>,
    max_results: usize,
) -> bool {
    let mut truncated = false;

    for (line_index, line) in content.lines().enumerate() {
        for found in matcher.find(line) {
            if results.len() >= max_results {
                truncated = true;
                return truncated;
            }

            results.push(SourceSearchMatch {
                path: path.to_string(),
                display_path: path.to_string(),
                line: line_index + 1,
                line_text: truncate_search_line(line),
                match_start: found.start(),
                match_end: found.end(),
            });
        }
    }

    truncated
}

fn truncate_search_line(line: &str) -> String {
    if line.len() <= MAX_SEARCH_LINE_BYTES {
        return line.to_string();
    }

    let mut end = MAX_SEARCH_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

fn discover_live_browsable_files(
    conf: &Val,
    project: &Project,
) -> Result<Vec<LocalSourceEntry>, String> {
    let mut files = Vec::new();
    let mut seen_public = ahash::AHashSet::new();

    discover_live_hot_files(conf, project, &mut files, &mut seen_public);
    discover_live_resource_files(conf, project, &mut files, &mut seen_public);

    files.sort_by(|a, b| a.public.path.cmp(&b.public.path));
    Ok(files)
}

fn discover_live_hot_files(
    conf: &Val,
    project: &Project,
    files: &mut Vec<LocalSourceEntry>,
    seen_public: &mut ahash::AHashSet<String>,
) {
    let opts = hot::discovery::DiscoveryOpts::for_extension("hot");
    for root_str in hot::project::get_project_src_paths(conf, &project.name) {
        let root = PathBuf::from(&root_str);
        if !root.exists() {
            continue;
        }

        for found in hot::discovery::discover(&[&root], &opts) {
            let abs_path = found
                .abs_path
                .canonicalize()
                .unwrap_or_else(|_| found.abs_path.clone());
            let path = format!("hot/src/{}", found.rel_path);
            if !seen_public.insert(path.clone()) {
                continue;
            }
            let size = std::fs::metadata(&abs_path).map(|m| m.len()).unwrap_or(0);
            let source_kind = source_kind_for_path(&path);
            files.push(LocalSourceEntry {
                public: SourceFileEntry {
                    name: file_name(&path),
                    path,
                    size,
                    source_kind,
                },
                abs_path,
            });
        }
    }
}

fn discover_live_resource_files(
    conf: &Val,
    project: &Project,
    files: &mut Vec<LocalSourceEntry>,
    seen_public: &mut ahash::AHashSet<String>,
) {
    let resource_roots = hot::project::get_project_resource_paths(conf, &project.name)
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if resource_roots.is_empty() {
        return;
    }

    let respect_gitignore = hot::project::get_project_respect_gitignore(conf, &project.name);
    let mut excludes = hot::project::get_project_ignore_excludes(conf, &project.name);
    excludes.extend(hot::project::get_project_resource_excludes(
        conf,
        &project.name,
    ));
    let registry =
        hot::lang::hot::resource::build_registry(&resource_roots, respect_gitignore, &excludes);

    for entry in registry.entries.values() {
        let size = std::fs::metadata(&entry.abs_path)
            .map(|m| m.len())
            .unwrap_or(entry.size);
        let path = format!("{RESOURCE_PREFIX}{}", normalize_separators(&entry.rel_path));
        if !seen_public.insert(path.clone()) || !is_browsable_entry(&path, size) {
            continue;
        }

        files.push(LocalSourceEntry {
            public: SourceFileEntry {
                name: file_name(&path),
                source_kind: source_kind_for_path(&path),
                path,
                size,
            },
            abs_path: entry.abs_path.clone(),
        });
    }
}

fn list_bundle_source_files(build_data: &[u8]) -> Result<Vec<SourceFileEntry>, String> {
    let cursor = Cursor::new(build_data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("failed to read build zip: {e}"))?;
    let mut files = Vec::new();

    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| format!("failed to read zip entry: {e}"))?;
        let name = file.name().to_string();
        if file.is_file() && is_browsable_entry(&name, file.size()) {
            files.push(SourceFileEntry {
                name: file_name(&name),
                source_kind: source_kind_for_path(&name),
                path: name,
                size: file.size(),
            });
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn resolve_bundle_source_path(
    build_data: &[u8],
    requested_path: &str,
) -> Result<Option<String>, String> {
    let candidates = list_bundle_source_files(build_data)?
        .into_iter()
        .map(|entry| entry.path)
        .collect::<Vec<_>>();
    Ok(resolve_source_path_from_candidates(
        requested_path,
        &candidates,
    ))
}

fn read_bundle_source_content(build_data: &[u8], path: &str) -> Result<String, String> {
    let cursor = Cursor::new(build_data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("failed to read build zip: {e}"))?;
    let mut file = archive
        .by_name(path)
        .map_err(|_| "source file not found in build".to_string())?;
    if !is_browsable_entry(path, file.size()) {
        return Err("requested path is not a browsable text entry".to_string());
    }
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| format!("failed to read source as UTF-8: {e}"))?;
    Ok(content)
}

fn resolve_live_source_entry<'a>(
    files: &'a [LocalSourceEntry],
    requested_path: &str,
) -> Option<&'a LocalSourceEntry> {
    let requested_norm = normalize_separators(requested_path);
    if requested_norm.trim().is_empty() {
        return None;
    }
    let requested_canonical = Path::new(requested_path).canonicalize().ok();

    if let Some(exact) = files
        .iter()
        .find(|entry| entry.public.path == requested_norm)
    {
        return Some(exact);
    }

    if let Some(canonical) = &requested_canonical
        && let Some(exact) = files.iter().find(|entry| canonical == &entry.abs_path)
    {
        return Some(exact);
    }

    let public_path_matches = files
        .iter()
        .filter(|entry| requested_norm.ends_with(&entry.public.path))
        .collect::<Vec<_>>();
    if public_path_matches.len() == 1 {
        return Some(public_path_matches[0]);
    }

    let suffix_matches = files
        .iter()
        .filter(|entry| {
            let abs_norm = normalize_separators(&entry.abs_path.to_string_lossy());
            abs_norm.ends_with(&requested_norm)
                || requested_norm.ends_with(file_name(&entry.public.path).as_str())
        })
        .collect::<Vec<_>>();

    if suffix_matches.len() == 1 {
        Some(suffix_matches[0])
    } else {
        None
    }
}

fn resolve_source_path_from_candidates(
    requested_path: &str,
    candidates: &[String],
) -> Option<String> {
    let requested_norm = normalize_separators(requested_path);
    if requested_norm.trim().is_empty() {
        return None;
    }

    if let Some(exact) = candidates
        .iter()
        .find(|candidate| **candidate == requested_norm)
    {
        return Some(exact.clone());
    }

    let matches = candidates
        .iter()
        .filter(|candidate| {
            requested_norm.ends_with(candidate.as_str())
                || candidate.ends_with(requested_norm.as_str())
                || requested_norm.ends_with(file_name(candidate).as_str())
        })
        .collect::<Vec<_>>();

    if matches.len() == 1 {
        Some((*matches[0]).clone())
    } else {
        None
    }
}

fn is_browsable_entry(path: &str, size: u64) -> bool {
    size <= MAX_BROWSABLE_FILE_BYTES && is_browsable_text_path(path)
}

fn is_browsable_text_path(path: &str) -> bool {
    let normalized = normalize_separators(path);
    let is_source = SOURCE_PREFIXES
        .iter()
        .any(|prefix| normalized.starts_with(prefix));
    let is_resource = normalized.starts_with(RESOURCE_PREFIX);
    (is_source || is_resource) && is_text_like_path(&normalized)
}

fn is_text_like_path(path: &str) -> bool {
    let lower = file_name(path).to_lowercase();
    matches!(lower.as_str(), "dockerfile")
        || lower.ends_with(".hot")
        || lower.ends_with(".skill.md")
        || lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.ends_with(".prompt")
        || lower.ends_with(".tmpl")
        || lower.ends_with(".py")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".sh")
        || lower.ends_with(".bash")
        || lower.ends_with(".ps1")
        || lower.ends_with(".json")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".toml")
        || lower.ends_with(".env")
        || lower.ends_with(".ini")
        || lower.ends_with(".html")
        || lower.ends_with(".css")
        || lower.ends_with(".xml")
        || lower.ends_with(".svg")
        || lower.ends_with(".dockerfile")
}

fn source_kind_for_path(path: &str) -> String {
    let normalized = normalize_separators(path);
    if normalized.starts_with("resources/") {
        "resource".to_string()
    } else if normalized.starts_with("hot/src/_skills/") {
        "generated".to_string()
    } else if normalized.starts_with("hot/pkg/") {
        "package".to_string()
    } else {
        "source".to_string()
    }
}

fn language_for_path(path: &str) -> String {
    let lower = file_name(path).to_lowercase();
    if lower.ends_with(".hot") {
        "hot".to_string()
    } else if lower.ends_with(".skill.md") || lower.ends_with(".md") {
        "markdown".to_string()
    } else if lower.ends_with(".json") {
        "json".to_string()
    } else if lower.ends_with(".js") || lower.ends_with(".jsx") {
        "javascript".to_string()
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") {
        "typescript".to_string()
    } else if lower.ends_with(".py") {
        "python".to_string()
    } else if lower.ends_with(".sh") || lower.ends_with(".bash") {
        "bash".to_string()
    } else if lower.ends_with(".ps1") {
        "powershell".to_string()
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        "yaml".to_string()
    } else if lower.ends_with(".toml") {
        "toml".to_string()
    } else if lower.ends_with(".html") {
        "html".to_string()
    } else if lower.ends_with(".css") {
        "css".to_string()
    } else if lower.ends_with(".xml") || lower.ends_with(".svg") {
        "xml".to_string()
    } else {
        "plain".to_string()
    }
}

fn file_name(path: &str) -> String {
    normalize_separators(path)
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsable_entry_allows_sources_and_text_resources() {
        assert!(is_browsable_entry("hot/src/app/main.hot", 10));
        assert!(is_browsable_entry("hot/pkg/example/lib.hot", 10));
        assert!(is_browsable_entry(
            "resources/skills/group-summary.skill.md",
            10
        ));
        assert!(is_browsable_entry("resources/scripts/fetcher.py", 10));
        assert!(!is_browsable_entry("manifest.hot", 10));
        assert!(!is_browsable_entry("docs/project/docs.json", 10));
        assert!(!is_browsable_entry("resources/video/demo.mp4", 10));
        assert!(!is_browsable_entry(
            "resources/large.md",
            MAX_BROWSABLE_FILE_BYTES + 1
        ));
    }

    #[test]
    fn language_for_path_maps_common_resource_types() {
        assert_eq!(language_for_path("resources/a.skill.md"), "markdown");
        assert_eq!(language_for_path("resources/a.md"), "markdown");
        assert_eq!(language_for_path("resources/a.py"), "python");
        assert_eq!(language_for_path("resources/a.ts"), "typescript");
        assert_eq!(language_for_path("resources/a.sh"), "bash");
    }

    #[test]
    fn resolve_source_path_prefers_exact_bundle_path() {
        let candidates = vec![
            "hot/src/a/main.hot".to_string(),
            "hot/src/b/main.hot".to_string(),
        ];
        assert_eq!(
            resolve_source_path_from_candidates("hot/src/a/main.hot", &candidates),
            Some("hot/src/a/main.hot".to_string())
        );
    }

    #[test]
    fn resolve_source_path_supports_absolute_legacy_suffix() {
        let candidates = vec!["hot/src/team-agent/agent.hot".to_string()];
        assert_eq!(
            resolve_source_path_from_candidates(
                "/Users/example/project/hot/src/team-agent/agent.hot",
                &candidates
            ),
            Some("hot/src/team-agent/agent.hot".to_string())
        );
    }

    #[test]
    fn resolve_source_path_rejects_ambiguous_basename() {
        let candidates = vec![
            "hot/src/a/main.hot".to_string(),
            "hot/src/b/main.hot".to_string(),
        ];
        assert_eq!(
            resolve_source_path_from_candidates("main.hot", &candidates),
            None
        );
    }

    #[test]
    fn resolve_live_source_entry_rejects_ambiguous_basename() {
        let files = vec![
            LocalSourceEntry {
                public: SourceFileEntry {
                    path: "hot/src/a/main.hot".to_string(),
                    name: "main.hot".to_string(),
                    size: 1,
                    source_kind: "source".to_string(),
                },
                abs_path: PathBuf::from("/tmp/project/a/main.hot"),
            },
            LocalSourceEntry {
                public: SourceFileEntry {
                    path: "hot/src/b/main.hot".to_string(),
                    name: "main.hot".to_string(),
                    size: 1,
                    source_kind: "source".to_string(),
                },
                abs_path: PathBuf::from("/tmp/project/b/main.hot"),
            },
        ];

        assert!(resolve_live_source_entry(&files, "main.hot").is_none());
    }

    #[test]
    fn search_source_content_supports_case_insensitive_plain_text() {
        let matcher = SourceSearchMatcher::new("pipe", false, false).unwrap();
        let mut results = Vec::new();
        let truncated = search_source_content(
            "hot/src/main.hot",
            "Pipe\nno match\npipeline",
            &matcher,
            &mut results,
            20,
        );

        assert!(!truncated);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].line, 1);
        assert_eq!(results[1].line, 3);
    }

    #[test]
    fn search_source_content_supports_regex() {
        let matcher = SourceSearchMatcher::new(r"order-\d+", true, true).unwrap();
        let mut results = Vec::new();
        search_source_content(
            "hot/src/main.hot",
            "order-123\norder-abc",
            &matcher,
            &mut results,
            20,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_start, 0);
        assert_eq!(results[0].match_end, 9);
    }
}
