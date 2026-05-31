use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::response::{Html, Json};
use axum::routing::get;
use hot::pkg::docs::{self, PkgDocs, PkgSummary, PkgVersionsIndex};
use hot::pkg::docs_html::{NavItem, TocItem, TocSection};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::services::ServeDir;

const DEFAULT_ORG: &str = "hot.dev";

#[derive(Clone, Debug)]
pub struct DocsConfig {
    pub docs_dir: PathBuf,
    pub docs_overlay_dir: Option<PathBuf>,
    pub app_assets_dir: PathBuf,
    pub pkg_docs_dir: PathBuf,
    pub pkg_source_dir: PathBuf,
    pub docs_examples_dir: PathBuf,
    pub github_url: String,
    pub hot_dev_url: String,
}

impl DocsConfig {
    pub fn from_resources() -> Self {
        let resources_dir =
            hot::resources::get_resources_path().unwrap_or_else(|_| PathBuf::from("resources"));

        Self {
            docs_dir: resources_dir.join("docs"),
            docs_overlay_dir: None,
            app_assets_dir: resources_dir.join("app/assets"),
            pkg_docs_dir: resources_dir.join("pkg-docs"),
            pkg_source_dir: PathBuf::from("hot/pkg"),
            docs_examples_dir: PathBuf::from("hot/test/docs"),
            github_url: "https://github.com/hot-dev/hot".to_string(),
            hot_dev_url: "https://hot.dev".to_string(),
        }
    }

    pub fn with_docs_overlay_dir(mut self, docs_overlay_dir: impl Into<PathBuf>) -> Self {
        self.docs_overlay_dir = Some(docs_overlay_dir.into());
        self
    }
}

#[derive(Clone, Debug)]
struct DocsState {
    config: DocsConfig,
}

pub fn preview_router() -> Router {
    let config = DocsConfig::from_resources();
    Router::new()
        .route("/", get(preview_landing_handler))
        .merge(router_with_config(config))
}

pub fn router() -> Router {
    router_with_config(DocsConfig::from_resources())
}

pub fn router_with_config(config: DocsConfig) -> Router {
    ensure_pkg_docs_generated(&config);

    let docs_assets_dir = config
        .docs_overlay_dir
        .as_ref()
        .filter(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| config.docs_dir.clone());
    let app_assets_dir = config.app_assets_dir.clone();
    let state = Arc::new(DocsState { config });

    Router::new()
        .route("/docs", get(docs_index_handler))
        .route("/docs/", get(docs_index_handler))
        .route("/docs/{*path}", get(docs_page_handler))
        .route("/pkg", get(pkg_index_handler))
        .route("/pkg/", get(pkg_index_handler))
        .route("/pkg/{*pkg_path}", get(pkg_docs_route_handler))
        .route("/api/search/docs", get(search_docs_handler))
        .route("/api/search/pkg/{org}/{pkg_name}", get(search_pkg_handler))
        .route("/api/packages", get(list_packages_handler))
        .route("/api/search/packages", get(search_all_packages_handler))
        .nest_service("/assets", ServeDir::new(app_assets_dir))
        .nest_service("/docs/assets", ServeDir::new(docs_assets_dir))
        .with_state(state)
}

async fn preview_landing_handler() -> Html<String> {
    landing_html(&DocsConfig::from_resources())
}

fn landing_html(config: &DocsConfig) -> Html<String> {
    Html(layout(
        config,
        "Hot Documentation",
        "docs-home",
        r#"
        <section class="hero">
            <p class="eyebrow">Hot Docs Preview</p>
            <h1>Preview official Hot documentation locally.</h1>
            <p>Edit docs and package docs in the public repository, then review them here before opening a pull request.</p>
        </section>
        <section class="panel-grid">
            <a class="panel" href="/docs">
                <span>Hot Docs</span>
                <strong>Language, platform, CLI, and integration guides</strong>
            </a>
            <a class="panel" href="/pkg">
                <span>Hot Packages</span>
                <strong>Generated package API documentation</strong>
            </a>
        </section>
        "#,
    ))
}

async fn docs_index_handler(State(state): State<Arc<DocsState>>) -> Html<String> {
    render_docs_page(state, "index").await
}

async fn docs_page_handler(
    State(state): State<Arc<DocsState>>,
    AxumPath(path): AxumPath<String>,
) -> Html<String> {
    render_docs_page(state, &path).await
}

async fn render_docs_page(state: Arc<DocsState>, path: &str) -> Html<String> {
    let nav = match load_nav(&state.config) {
        Ok(nav) => nav,
        Err(error) => return Html(error_page(&state.config, "Documentation error", &error)),
    };

    let page = match load_page(&state.config, path) {
        Ok(page) => page,
        Err(error) => return Html(error_page(&state.config, "Page not found", &error)),
    };

    let (prev, next) = get_prev_next(&nav, &page.path);
    let breadcrumb = find_nav_path(&nav, &page.path);

    let body = format!(
        r#"
        <div class="docs-shell">
            <aside class="sidebar">{}</aside>
            <main class="content docs-content">
                {}
                {}
                <nav class="pager">
                    {}
                    {}
                </nav>
            </main>
            <aside class="toc">{}</aside>
        </div>
        "#,
        render_nav(&nav, "/docs", &page.path),
        render_breadcrumb(&breadcrumb),
        page.content_html,
        render_pager("Previous", prev.as_ref(), "/docs"),
        render_pager("Next", next.as_ref(), "/docs"),
        render_toc(&page.toc),
    );

    Html(layout(&state.config, &page.title, "docs", &body))
}

async fn pkg_index_handler(State(state): State<Arc<DocsState>>) -> Html<String> {
    let packages = list_packages(&state.config);
    let mut tags: Vec<String> = packages
        .iter()
        .flat_map(|package| package.tags.iter().cloned())
        .collect();
    tags.sort();
    tags.dedup();

    let tags_html = tags
        .iter()
        .map(|tag| format!(r#"<span class="tag">{}</span>"#, escape_html(tag)))
        .collect::<Vec<_>>()
        .join("");
    let packages_html = packages
        .iter()
        .map(|package| {
            let pkg_name = package.name.split('/').next_back().unwrap_or(&package.name);
            format!(
                r#"<a class="package-card" href="/pkg/{}/{}"><strong>{}</strong><span>{}</span><small>{}</small></a>"#,
                DEFAULT_ORG,
                escape_attr(pkg_name),
                escape_html(&package.display_name),
                escape_html(&package.description),
                escape_html(&package.version),
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let body = format!(
        r#"
        <section class="hero compact">
            <p class="eyebrow">Hot Packages</p>
            <h1>Package documentation</h1>
            <p>Generated API documentation for published Hot packages.</p>
        </section>
        <div class="tags">{tags_html}</div>
        <section class="package-grid">{packages_html}</section>
        "#
    );
    Html(layout(&state.config, "Hot Packages", "pkg", &body))
}

async fn pkg_docs_route_handler(
    State(state): State<Arc<DocsState>>,
    AxumPath(pkg_path): AxumPath<String>,
) -> Html<String> {
    let parts: Vec<&str> = pkg_path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 2 {
        return Html(error_page(
            &state.config,
            "Package not found",
            "Expected a package path like /pkg/hot.dev/hot-std.",
        ));
    }

    let org = parts[0];
    let (pkg_name, requested_version) = parse_pkg_version(parts[1]);
    let module_path = parts
        .get(2..)
        .map(|parts| parts.join("/"))
        .unwrap_or_default();

    if org != DEFAULT_ORG {
        return Html(error_page(
            &state.config,
            "Package not found",
            &format!("Unknown package organization: {org}"),
        ));
    }

    let page = match load_pkg_page(&state.config, pkg_name, requested_version, &module_path) {
        Ok(page) => page,
        Err(error) => return Html(error_page(&state.config, "Package not found", &error)),
    };

    let body = format!(
        r#"
        <div class="docs-shell">
            <aside class="sidebar">{}</aside>
            <main class="content docs-content">{}</main>
            <aside class="toc">
                <div class="version">Version {}</div>
                {}
            </aside>
        </div>
        "#,
        render_nav(&page.nav, "/pkg", &module_path),
        page.html,
        escape_html(&page.current_version),
        render_toc(&page.toc),
    );

    Html(layout(&state.config, &page.title, "pkg", &body))
}

async fn list_packages_handler(State(state): State<Arc<DocsState>>) -> Json<Value> {
    Json(json!({ "packages": list_packages(&state.config) }))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

async fn search_docs_handler(
    State(state): State<Arc<DocsState>>,
    Query(query): Query<SearchQuery>,
) -> Json<Value> {
    let q = query.q.unwrap_or_default().to_lowercase();
    let results = load_all_pages(&state.config)
        .unwrap_or_default()
        .into_iter()
        .filter(|page| {
            q.is_empty()
                || page.title.to_lowercase().contains(&q)
                || page.content_text.to_lowercase().contains(&q)
        })
        .take(20)
        .map(|page| json!({ "title": page.title, "url": format!("/docs/{}", page.path), "excerpt": page.excerpt }))
        .collect::<Vec<_>>();
    Json(json!({ "results": results }))
}

async fn search_pkg_handler(
    State(state): State<Arc<DocsState>>,
    AxumPath((_org, pkg_name)): AxumPath<(String, String)>,
    Query(query): Query<SearchQuery>,
) -> Json<Value> {
    let q = query.q.unwrap_or_default().to_lowercase();
    let (pkg_name, version) = parse_pkg_version(&pkg_name);
    let (docs, _, _) = match load_cached_pkg_docs_versioned(&state.config, pkg_name, version) {
        Ok(docs) => docs,
        Err(error) => return Json(json!({ "error": error, "results": [] })),
    };

    let mut results = Vec::new();
    for namespace in docs.namespaces {
        let title = namespace.namespace.clone();
        let body = namespace.doc.clone().unwrap_or_default();
        if q.is_empty() || title.to_lowercase().contains(&q) || body.to_lowercase().contains(&q) {
            results.push(json!({
                "title": title,
                "url": format!("/pkg/{}/{}/{}", DEFAULT_ORG, pkg_name, namespace.name),
                "excerpt": excerpt(&body),
            }));
        }
    }
    Json(json!({ "results": results }))
}

async fn search_all_packages_handler(
    State(state): State<Arc<DocsState>>,
    Query(query): Query<SearchQuery>,
) -> Json<Value> {
    let q = query.q.unwrap_or_default().to_lowercase();
    let results = list_packages(&state.config)
        .into_iter()
        .filter(|package| {
            q.is_empty()
                || package.display_name.to_lowercase().contains(&q)
                || package.description.to_lowercase().contains(&q)
                || package
                    .tags
                    .iter()
                    .any(|tag| tag.to_lowercase().contains(&q))
        })
        .take(20)
        .map(|package| {
            let pkg_name = package.name.split('/').next_back().unwrap_or(&package.name);
            json!({
                "title": package.display_name,
                "url": format!("/pkg/{}/{}", DEFAULT_ORG, pkg_name),
                "excerpt": package.description,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "results": results }))
}

#[derive(Debug, Clone)]
struct DocPage {
    title: String,
    content_html: String,
    content_text: String,
    path: String,
    excerpt: String,
    toc: Vec<TocSection>,
}

#[derive(Debug, Clone)]
struct VersionedPkgPage {
    title: String,
    html: String,
    nav: Vec<NavItem>,
    toc: Vec<TocSection>,
    current_version: String,
}

fn load_nav(config: &DocsConfig) -> Result<Vec<NavItem>, String> {
    let mut nav = load_nav_from_dir(&config.docs_dir)?;
    if let Some(overlay_dir) = &config.docs_overlay_dir
        && overlay_dir.exists()
    {
        nav.extend(load_nav_from_dir(overlay_dir).unwrap_or_default());
    }
    Ok(nav)
}

fn load_nav_from_dir(docs_dir: &Path) -> Result<Vec<NavItem>, String> {
    let nav_path = docs_dir.join("nav.md");
    if nav_path.exists() {
        let content = fs::read_to_string(&nav_path)
            .map_err(|e| format!("Failed to read {}: {e}", nav_path.display()))?;
        parse_nav_md(&content)
    } else {
        auto_discover_nav(docs_dir)
    }
}

fn parse_nav_md(content: &str) -> Result<Vec<NavItem>, String> {
    let mut items = Vec::new();
    let mut stack: Vec<(usize, NavItem)> = Vec::new();
    let mut in_comment = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("<!--") {
            in_comment = true;
        }
        if in_comment {
            if trimmed.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix('-') else {
            continue;
        };

        let indent = line.len() - line.trim_start().len();
        let level = indent / 2;
        let (title, path) = parse_nav_item(rest.trim());
        let item = NavItem {
            title,
            path,
            children: Vec::new(),
        };

        while let Some((stack_level, _)) = stack.last() {
            if *stack_level >= level {
                let (_, completed_item) = stack.pop().unwrap();
                if let Some((_, parent)) = stack.last_mut() {
                    parent.children.push(completed_item);
                } else {
                    items.push(completed_item);
                }
            } else {
                break;
            }
        }
        stack.push((level, item));
    }

    while let Some((_, completed_item)) = stack.pop() {
        if let Some((_, parent)) = stack.last_mut() {
            parent.children.push(completed_item);
        } else {
            items.push(completed_item);
        }
    }

    Ok(items)
}

fn parse_nav_item(item: &str) -> (String, Option<String>) {
    if item.starts_with('[')
        && let Some(end_bracket) = item.find(']')
    {
        let title = item[1..end_bracket].to_string();
        let path = item
            .find('(')
            .and_then(|start| item.find(')').map(|end| item[start + 1..end].to_string()));
        return (title, path);
    }
    (item.to_string(), None)
}

fn auto_discover_nav(docs_dir: &Path) -> Result<Vec<NavItem>, String> {
    if !docs_dir.exists() {
        return Err(format!(
            "Docs directory does not exist: {}",
            docs_dir.display()
        ));
    }
    let mut items = Vec::new();
    let mut entries = fs::read_dir(docs_dir)
        .map_err(|e| format!("Failed to read docs directory {}: {e}", docs_dir.display()))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "nav.md" {
            continue;
        }
        if path.is_dir() && path.join("index.md").exists() {
            items.push(NavItem {
                title: title_from_slug(&name),
                path: Some(name),
                children: Vec::new(),
            });
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            let slug = name.trim_end_matches(".md").to_string();
            items.push(NavItem {
                title: title_from_slug(&slug),
                path: Some(slug),
                children: Vec::new(),
            });
        }
    }

    Ok(items)
}

fn load_page(config: &DocsConfig, path: &str) -> Result<DocPage, String> {
    let normalized = normalize_docs_path(path);
    if let Some(overlay_dir) = &config.docs_overlay_dir
        && overlay_dir.exists()
        && let Ok(page) = load_page_from_dir(config, overlay_dir, &normalized)
    {
        return Ok(page);
    }
    load_page_from_dir(config, &config.docs_dir, &normalized)
}

fn load_page_from_dir(config: &DocsConfig, docs_dir: &Path, path: &str) -> Result<DocPage, String> {
    let file_path = docs_file_path(docs_dir, path)
        .ok_or_else(|| format!("The documentation page '{path}' was not found."))?;
    let markdown = fs::read_to_string(&file_path)
        .map_err(|e| format!("Failed to read {}: {e}", file_path.display()))?;
    let markdown = strip_frontmatter(&markdown);
    let markdown = process_snippets(config, markdown);
    let title = extract_title(&markdown).unwrap_or_else(|| title_from_slug(path));
    let (content_html, toc) = markdown_to_html_with_toc(&markdown);
    let content_text = strip_markdown(&markdown);
    let excerpt = excerpt(&content_text);

    Ok(DocPage {
        title,
        content_html,
        content_text,
        path: path.to_string(),
        excerpt,
        toc,
    })
}

fn strip_frontmatter(markdown: &str) -> &str {
    if !markdown.starts_with("---\n") {
        return markdown;
    }

    let Some(end_idx) = markdown[4..].find("\n---") else {
        return markdown;
    };

    markdown.get(end_idx + 8..).unwrap_or("").trim_start()
}

fn docs_file_path(docs_dir: &Path, path: &str) -> Option<PathBuf> {
    let cleaned = path.trim_matches('/').trim_end_matches(".md");
    let candidates = if cleaned == "index" || cleaned.is_empty() {
        vec![docs_dir.join("index.md")]
    } else {
        vec![
            docs_dir.join(format!("{cleaned}.md")),
            docs_dir.join(cleaned).join("index.md"),
        ]
    };
    candidates.into_iter().find(|candidate| candidate.exists())
}

fn load_all_pages(config: &DocsConfig) -> Result<Vec<DocPage>, String> {
    let mut pages = Vec::new();
    collect_pages_from_dir(config, &config.docs_dir, &config.docs_dir, &mut pages)?;
    if let Some(overlay_dir) = &config.docs_overlay_dir
        && overlay_dir.exists()
    {
        collect_pages_from_dir(config, overlay_dir, overlay_dir, &mut pages)?;
    }
    Ok(pages)
}

fn collect_pages_from_dir(
    config: &DocsConfig,
    root: &Path,
    dir: &Path,
    pages: &mut Vec<DocPage>,
) -> Result<(), String> {
    for entry in fs::read_dir(dir)
        .map_err(|e| format!("Failed to read docs directory {}: {e}", dir.display()))?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "nav.md" {
            continue;
        }
        if path.is_dir() {
            collect_pages_from_dir(config, root, &path, pages)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let normalized = if rel == "index" {
                "index".to_string()
            } else {
                rel.trim_end_matches("/index").to_string()
            };
            if let Ok(page) = load_page_from_dir(config, root, &normalized) {
                pages.push(page);
            }
        }
    }
    Ok(())
}

fn normalize_docs_path(path: &str) -> String {
    let path = path.trim_matches('/').trim_end_matches(".md");
    if path.is_empty() {
        "index".to_string()
    } else {
        path.to_string()
    }
}

fn process_snippets(config: &DocsConfig, markdown: &str) -> String {
    let snippet_re =
        Regex::new(r"\{\{snippet:([a-zA-Z0-9_-]+)#([a-zA-Z0-9_-]+)(:eval)?\}\}").unwrap();
    let result_re = Regex::new(r"\{\{result:([a-zA-Z0-9_-]+)#([a-zA-Z0-9_-]+)\}\}").unwrap();

    let with_snippets = snippet_re
        .replace_all(markdown, |caps: &regex::Captures| {
            extract_block(config, &caps[1], &caps[2], "doc", false)
                .map(|code| format!("```hot\n{code}\n```"))
                .unwrap_or_else(|| {
                    format!(
                        "```hot\n// ERROR: Snippet not found: {}#{}\n```",
                        &caps[1], &caps[2]
                    )
                })
        })
        .to_string();

    result_re
        .replace_all(&with_snippets, |caps: &regex::Captures| {
            extract_block(config, &caps[1], &caps[2], "result", true)
                .map(|code| format!("```result\n{code}\n```"))
                .unwrap_or_else(|| {
                    format!(
                        "```result\n// ERROR: Result not found: {}#{}\n```",
                        &caps[1], &caps[2]
                    )
                })
        })
        .to_string()
}

fn extract_block(
    config: &DocsConfig,
    file: &str,
    name: &str,
    marker_type: &str,
    strip_comment_prefix: bool,
) -> Option<String> {
    let content = fs::read_to_string(config.docs_examples_dir.join(format!("{file}.hot"))).ok()?;
    let start_marker = format!("// @{marker_type}: {name}");
    let end_marker = format!("// @end: {name}");
    let mut in_block = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if line.trim() == start_marker {
            in_block = true;
            continue;
        }
        if in_block && line.trim() == end_marker {
            break;
        }
        if in_block {
            let line = if strip_comment_prefix {
                line.strip_prefix("// ").unwrap_or(line)
            } else {
                line
            };
            lines.push(line);
        }
    }

    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn markdown_to_html_with_toc(markdown: &str) -> (String, Vec<TocSection>) {
    let mut html_output = String::new();
    let mut toc_items = Vec::new();
    let mut events = Vec::new();
    let mut current_heading: Option<(u8, String)> = None;
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_block_content = String::new();

    let parser = Parser::new_ext(markdown, Options::all());
    for event in parser {
        if in_code_block {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    events.push(Event::Html(
                        render_code_block(&code_block_lang, &code_block_content).into(),
                    ));
                    code_block_lang.clear();
                    code_block_content.clear();
                }
                Event::Text(text) | Event::Code(text) => code_block_content.push_str(&text),
                Event::SoftBreak | Event::HardBreak => code_block_content.push('\n'),
                _ => {}
            }
            continue;
        }

        match &event {
            Event::Start(Tag::Heading { level, .. }) => {
                current_heading = Some((*level as u8, String::new()));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text)) = current_heading.take() {
                    let anchor = slugify(&text);
                    toc_items.push(TocItem {
                        name: text.clone(),
                        anchor: anchor.clone(),
                        is_core: false,
                        indent: level.saturating_sub(2),
                        badges: Vec::new(),
                    });
                    events.push(Event::Html(format!(r#"<a id="{anchor}"></a>"#).into()));
                }
            }
            Event::Text(text) => {
                if let Some((_, heading_text)) = current_heading.as_mut() {
                    heading_text.push_str(text);
                }
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
                in_code_block = true;
                code_block_lang = lang.to_string();
                code_block_content.clear();
                continue;
            }
            _ => {}
        }
        events.push(event);
    }

    html::push_html(&mut html_output, events.into_iter());
    if in_code_block {
        tracing::debug!("markdown parser ended while in a code block");
    }

    (
        html_output,
        vec![TocSection {
            title: "On this page".to_string(),
            items: toc_items,
        }],
    )
}

fn render_code_block(lang: &str, code: &str) -> String {
    let lang = lang.split_whitespace().next().unwrap_or_default();
    if lang == "result" {
        return format!(
            r#"<div class="result-block"><pre><code class="language-plaintext">{}</code></pre></div>"#,
            escape_html(code)
        );
    }

    let lang = if lang.is_empty() { "plaintext" } else { lang };
    format!(
        r#"<pre><code class="language-{}">{}</code></pre>"#,
        escape_attr(lang),
        escape_html(code)
    )
}

fn extract_title(markdown: &str) -> Option<String> {
    markdown.lines().find_map(|line| {
        line.strip_prefix("# ")
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(ToString::to_string)
    })
}

fn strip_markdown(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, Options::all());
    parser
        .filter_map(|event| match event {
            Event::Text(text) | Event::Code(text) => Some(text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn list_packages(config: &DocsConfig) -> Vec<PkgSummary> {
    let mut packages = hot::pkg::docs::list_all_packages();
    if packages.is_empty()
        && config.pkg_docs_dir.exists()
        && let Ok(entries) = fs::read_dir(&config.pkg_docs_dir)
    {
        for entry in entries.filter_map(Result::ok) {
            let pkg_name = entry.file_name().to_string_lossy().to_string();
            let Ok((docs, version, _)) = load_cached_pkg_docs_versioned(config, &pkg_name, None)
            else {
                continue;
            };
            packages.push(PkgSummary {
                name: docs.meta.name.clone(),
                display_name: pkg_name,
                version,
                description: docs.meta.description.clone(),
                tags: docs.meta.tags.clone(),
                namespace_count: docs.namespaces.len(),
                function_count: docs
                    .namespaces
                    .iter()
                    .map(|namespace| namespace.functions.len())
                    .sum(),
                type_count: docs
                    .namespaces
                    .iter()
                    .map(|namespace| namespace.types.len())
                    .sum(),
            });
        }
    }
    if packages.is_empty() {
        packages.extend(list_source_packages(config));
    }
    sort_packages(&mut packages);
    packages
}

fn ensure_pkg_docs_generated(config: &DocsConfig) {
    let package_names = source_package_names(config);
    if package_names.is_empty() {
        return;
    }

    let force_rebuild = truthy_env("HOT_REBUILD_DOCS");
    let packages_to_generate = package_names
        .iter()
        .filter(|pkg_name| {
            force_rebuild
                || !config
                    .pkg_docs_dir
                    .join(pkg_name.as_str())
                    .join("versions.json")
                    .exists()
        })
        .cloned()
        .collect::<Vec<_>>();

    if packages_to_generate.is_empty() {
        return;
    }

    if let Err(error) = fs::create_dir_all(&config.pkg_docs_dir) {
        tracing::warn!(
            "Failed to create package docs directory {}: {error}",
            config.pkg_docs_dir.display()
        );
        return;
    }

    tracing::info!(
        "Generating docs for {} package(s) into {}",
        packages_to_generate.len(),
        config.pkg_docs_dir.display()
    );

    let served_package_names = package_names
        .iter()
        .map(|pkg_name| format!("{DEFAULT_ORG}/{pkg_name}"))
        .collect::<Vec<_>>();
    let served_packages = served_package_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();

    for pkg_name in packages_to_generate {
        let pkg_path = config.pkg_source_dir.join(&pkg_name);
        match generate_versioned_pkg_docs_from_path(
            &pkg_name,
            &pkg_path,
            &config.pkg_docs_dir,
            &served_packages,
        ) {
            Ok(version) => tracing::info!("Generated docs for {pkg_name}@{version}"),
            Err(error) => tracing::warn!("Failed to generate docs for {pkg_name}: {error}"),
        }
    }
}

fn source_package_names(config: &DocsConfig) -> Vec<String> {
    let Ok(entries) = fs::read_dir(&config.pkg_source_dir) else {
        return Vec::new();
    };

    let mut package_names = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pkg_path = entry.path();
            if pkg_path.is_dir() && pkg_path.join("pkg.hot").exists() {
                entry.file_name().into_string().ok()
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    package_names.sort();
    package_names
}

fn generate_versioned_pkg_docs_from_path(
    pkg_name: &str,
    pkg_path: &Path,
    out_dir: &Path,
    served_packages: &[&str],
) -> Result<String, String> {
    let mut pkg_docs = docs::load_pkg_docs(pkg_path)?;
    let version = pkg_docs.meta.version.clone();
    if version.is_empty() {
        return Err(format!(
            "Package '{pkg_name}' has no version defined in pkg.hot"
        ));
    }

    for dep in &mut pkg_docs.meta.deps {
        if served_packages.contains(&dep.name.as_str()) {
            dep.docs_url = Some(format!("/pkg/{}", dep.name));
        }
    }

    let pkg_dir = out_dir.join(pkg_name);
    let version_dir = pkg_dir.join(&version);
    fs::create_dir_all(&version_dir)
        .map_err(|e| format!("Failed to create docs directory: {e}"))?;

    let docs_json = serde_json::to_string_pretty(&pkg_docs)
        .map_err(|e| format!("Failed to serialize docs: {e}"))?;
    fs::write(version_dir.join("docs.json"), docs_json)
        .map_err(|e| format!("Failed to write docs file: {e}"))?;

    let versions_path = pkg_dir.join("versions.json");
    let mut versions_index =
        docs::load_versions_index(&versions_path).unwrap_or_else(|_| PkgVersionsIndex {
            latest: version.clone(),
            versions: vec![],
        });
    if !versions_index.versions.contains(&version) {
        versions_index.versions.push(version.clone());
    }
    versions_index.latest = version.clone();
    versions_index.versions.sort_by(|a, b| b.cmp(a));

    let versions_json = serde_json::to_string_pretty(&versions_index)
        .map_err(|e| format!("Failed to serialize versions: {e}"))?;
    fs::write(versions_path, versions_json)
        .map_err(|e| format!("Failed to write versions file: {e}"))?;

    Ok(version)
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .map(|value| !value.is_empty() && value != "0" && value.to_lowercase() != "false")
        .unwrap_or(false)
}

fn list_source_packages(config: &DocsConfig) -> Vec<PkgSummary> {
    let Ok(entries) = fs::read_dir(&config.pkg_source_dir) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pkg_path = entry.path();
            if !pkg_path.is_dir() || !pkg_path.join("pkg.hot").exists() {
                return None;
            }

            let pkg_name = pkg_path.file_name()?.to_str()?;
            let Ok((docs, version, _)) = load_cached_pkg_docs_versioned(config, pkg_name, None)
            else {
                return None;
            };

            Some(PkgSummary {
                name: docs.meta.name.clone(),
                display_name: docs
                    .meta
                    .name
                    .split('/')
                    .next_back()
                    .unwrap_or(&docs.meta.name)
                    .to_string(),
                version,
                description: docs.meta.description.clone(),
                tags: docs.meta.tags.clone(),
                namespace_count: docs.namespaces.len(),
                function_count: docs
                    .namespaces
                    .iter()
                    .map(|namespace| namespace.functions.len())
                    .sum(),
                type_count: docs
                    .namespaces
                    .iter()
                    .map(|namespace| namespace.types.len())
                    .sum(),
            })
        })
        .collect()
}

fn sort_packages(packages: &mut [PkgSummary]) {
    packages.sort_by(|a, b| {
        if a.display_name == "hot-std" {
            std::cmp::Ordering::Less
        } else if b.display_name == "hot-std" {
            std::cmp::Ordering::Greater
        } else {
            a.display_name.cmp(&b.display_name)
        }
    });
}

fn parse_pkg_version(pkg_name_with_version: &str) -> (&str, Option<&str>) {
    if let Some(at_pos) = pkg_name_with_version.find('@') {
        (
            &pkg_name_with_version[..at_pos],
            Some(&pkg_name_with_version[at_pos + 1..]),
        )
    } else {
        (pkg_name_with_version, None)
    }
}

fn load_cached_pkg_docs_versioned(
    config: &DocsConfig,
    pkg_name: &str,
    version: Option<&str>,
) -> Result<(PkgDocs, String, Vec<String>), String> {
    let versions_path = config.pkg_docs_dir.join(pkg_name).join("versions.json");
    if versions_path.exists() {
        let versions_index: PkgVersionsIndex = docs::load_versions_index(&versions_path)?;
        let target_version = version.unwrap_or(&versions_index.latest);
        let docs_path = config
            .pkg_docs_dir
            .join(pkg_name)
            .join(target_version)
            .join("docs.json");
        if docs_path.exists() {
            let pkg_docs = docs::load_pkg_docs_from_json(&docs_path)?;
            return Ok((
                pkg_docs,
                target_version.to_string(),
                versions_index.versions,
            ));
        }
    }

    let legacy_path = config.pkg_docs_dir.join(format!("{pkg_name}.json"));
    if legacy_path.exists() {
        let pkg_docs = docs::load_pkg_docs_from_json(&legacy_path)?;
        let version = pkg_docs.meta.version.clone();
        return Ok((pkg_docs, version.clone(), vec![version]));
    }

    let pkg_path = config.pkg_source_dir.join(pkg_name);
    if pkg_path.exists() {
        let pkg_docs = docs::load_pkg_docs(&pkg_path)?;
        let version = pkg_docs.meta.version.clone();
        return Ok((pkg_docs, version.clone(), vec![version]));
    }

    let pkg_path =
        docs::find_pkg_path(pkg_name).ok_or_else(|| format!("Package '{pkg_name}' not found"))?;
    let pkg_docs = docs::load_pkg_docs(&pkg_path)?;
    let version = pkg_docs.meta.version.clone();
    Ok((pkg_docs, version.clone(), vec![version]))
}

fn load_pkg_page(
    config: &DocsConfig,
    pkg_name: &str,
    requested_version: Option<&str>,
    module_path: &str,
) -> Result<VersionedPkgPage, String> {
    let (pkg_docs, current_version, _versions) =
        load_cached_pkg_docs_versioned(config, pkg_name, requested_version)?;
    let full_path = format!("{DEFAULT_ORG}/{pkg_name}");
    let nav = hot::pkg::docs_html::generate_pkg_nav_with_prefix(pkg_name, &pkg_docs, &full_path);

    if module_path.is_empty() || module_path == "index" || module_path == "readme" {
        let html = if module_path == "readme" {
            render_pkg_readme(config, pkg_name)?
        } else {
            hot::pkg::docs_html::generate_pkg_index_html(&pkg_docs, &full_path, "/pkg")
        };
        let title = format!("{} - Hot Packages", pkg_docs.meta.name);
        return Ok(VersionedPkgPage {
            title,
            html,
            nav,
            toc: Vec::new(),
            current_version,
        });
    }

    let namespace = pkg_docs
        .namespaces
        .iter()
        .find(|namespace| namespace.name == module_path || namespace.namespace == module_path)
        .ok_or_else(|| format!("Namespace '{module_path}' not found in package '{pkg_name}'"))?;
    let html = hot::pkg::docs_html::generate_namespace_html_with_registry(
        namespace,
        &full_path,
        &pkg_docs.type_index,
        None,
        "/pkg",
    );
    let title = format!("{} - {}", namespace.namespace, pkg_docs.meta.name);
    Ok(VersionedPkgPage {
        title,
        html,
        nav,
        toc: Vec::new(),
        current_version,
    })
}

fn render_pkg_readme(config: &DocsConfig, pkg_name: &str) -> Result<String, String> {
    let readme_path = config.pkg_source_dir.join(pkg_name).join("README.md");
    let markdown = fs::read_to_string(&readme_path)
        .map_err(|e| format!("Failed to read {}: {e}", readme_path.display()))?;
    Ok(markdown_to_html_with_toc(&markdown).0)
}

fn get_prev_next(nav: &[NavItem], current_path: &str) -> (Option<NavItem>, Option<NavItem>) {
    let mut flat = Vec::new();
    flatten_nav(nav, &mut flat);
    let index = flat
        .iter()
        .position(|item| item.path.as_deref() == Some(current_path));
    if let Some(index) = index {
        (
            index.checked_sub(1).and_then(|i| flat.get(i)).cloned(),
            flat.get(index + 1).cloned(),
        )
    } else {
        (None, None)
    }
}

fn flatten_nav(items: &[NavItem], flat: &mut Vec<NavItem>) {
    for item in items {
        if item.path.is_some() {
            flat.push(item.clone());
        }
        flatten_nav(&item.children, flat);
    }
}

fn find_nav_path(nav: &[NavItem], current_path: &str) -> Vec<String> {
    fn find(items: &[NavItem], current_path: &str, path: &mut Vec<String>) -> bool {
        for item in items {
            path.push(item.title.clone());
            if item.path.as_deref() == Some(current_path)
                || find(&item.children, current_path, path)
            {
                return true;
            }
            path.pop();
        }
        false
    }

    let mut path = Vec::new();
    find(nav, current_path, &mut path);
    path
}

fn render_nav(nav: &[NavItem], base: &str, current_path: &str) -> String {
    fn render_items(items: &[NavItem], base: &str, current_path: &str) -> String {
        let mut html = String::from("<ul>");
        for item in items {
            let active = item.path.as_deref() == Some(current_path);
            let label = escape_html(&item.title);
            let row = if let Some(path) = &item.path {
                format!(
                    r#"<a class="{}" href="{}/{}">{}</a>"#,
                    if active { "active" } else { "" },
                    base,
                    escape_attr(path),
                    label
                )
            } else {
                format!(r#"<span>{label}</span>"#)
            };
            html.push_str("<li>");
            html.push_str(&row);
            if !item.children.is_empty() {
                html.push_str(&render_items(&item.children, base, current_path));
            }
            html.push_str("</li>");
        }
        html.push_str("</ul>");
        html
    }
    render_items(nav, base, current_path)
}

fn render_toc(toc: &[TocSection]) -> String {
    let mut html = String::new();
    for section in toc {
        if section.items.is_empty() {
            continue;
        }
        html.push_str(&format!(
            "<strong>{}</strong><ul>",
            escape_html(&section.title)
        ));
        for item in &section.items {
            html.push_str(&format!(
                r##"<li class="indent-{}"><a class="toc-link" href="#{}">{}</a></li>"##,
                item.indent,
                escape_attr(&item.anchor),
                escape_html(&item.name)
            ));
        }
        html.push_str("</ul>");
    }
    html
}

fn render_pager(label: &str, item: Option<&NavItem>, base: &str) -> String {
    if let Some(item) = item
        && let Some(path) = &item.path
    {
        return format!(
            r#"<a class="pager-link" href="{}/{}"><span>{}</span><strong>{}</strong></a>"#,
            base,
            escape_attr(path),
            label,
            escape_html(&item.title)
        );
    }
    String::new()
}

fn render_breadcrumb(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    format!(
        r#"<nav class="breadcrumb">{}</nav>"#,
        items
            .iter()
            .map(|item| escape_html(item))
            .collect::<Vec<_>>()
            .join(" / ")
    )
}

fn layout(config: &DocsConfig, title: &str, current_page: &str, body: &str) -> String {
    let docs_active = if current_page == "docs" { "active" } else { "" };
    let pkg_active = if current_page == "pkg" { "active" } else { "" };
    format!(
        r#"<!doctype html>
<html lang="en" class="dark">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{}</title>
  <link rel="stylesheet" href="/assets/css/prism-hot-theme.css">
  <link rel="stylesheet" href="/assets/css/hot-docs.css">
</head>
<body>
  <header>
    <a class="logo" href="/">Hot Dev</a>
    <nav>
      <a class="{docs_active}" href="/docs">Docs</a>
      <a class="{pkg_active}" href="/pkg">Packages</a>
      <a href="{}">hot.dev</a>
      <a href="{}">GitHub</a>
    </nav>
  </header>
  <main>{body}</main>
  <footer>
    <span>&copy; {} Hot Dev</span>
    <a href="{}">hot.dev</a>
    <a href="{}">GitHub</a>
  </footer>
  <script src="/assets/js/prism.js" defer></script>
  <script src="/assets/js/prism-hot.js" defer></script>
  <script src="/assets/js/hot-docs.js" defer></script>
</body>
</html>"#,
        escape_html(title),
        escape_attr(&config.hot_dev_url),
        escape_attr(&config.github_url),
        2026,
        escape_attr(&config.hot_dev_url),
        escape_attr(&config.github_url),
    )
}

fn error_page(config: &DocsConfig, title: &str, message: &str) -> String {
    layout(
        config,
        title,
        "error",
        &format!(
            r#"<section class="hero compact"><h1>{}</h1><p>{}</p><p><a href="/docs">Back to docs</a></p></section>"#,
            escape_html(title),
            escape_html(message)
        ),
    )
}

fn title_from_slug(slug: &str) -> String {
    slug.split(['-', '_', '/'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn excerpt(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() > 180 {
        format!("{}...", &text[..180])
    } else {
        text
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_attr(value: &str) -> String {
    escape_html(value).replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nav_markdown() {
        let nav = parse_nav_md(
            r#"
            # Docs
            - [Getting Started](getting-started)
            - [Language](language)
              - [Functions](language/functions)
            "#,
        )
        .unwrap();
        assert_eq!(nav.len(), 2);
        assert_eq!(nav[1].children.len(), 1);
    }

    #[test]
    fn normalizes_empty_docs_path_to_index() {
        assert_eq!(normalize_docs_path(""), "index");
        assert_eq!(normalize_docs_path("/language/"), "language");
    }

    #[test]
    fn renders_code_blocks_for_prism() {
        let (html, _) = markdown_to_html_with_toc("# Demo\n\n```hot\nmain fn () { null }\n```");
        assert!(html.contains(r#"class="language-hot""#));
        assert!(html.contains("main fn"));
    }

    #[test]
    fn renders_result_blocks() {
        let (html, _) = markdown_to_html_with_toc("```result\nok\n```");
        assert!(html.contains(r#"class="result-block""#));
        assert!(html.contains(r#"class="language-plaintext""#));
    }
}
