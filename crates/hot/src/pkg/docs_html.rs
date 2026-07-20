//! HTML rendering for package documentation
//!
//! This module provides shared HTML rendering functions for package documentation.

use super::docs::{BoxRequirementDoc, CtxRequirements, DocNamespace, PkgDocs};
use ahash::{AHashMap, AHashSet};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A navigation item for docs sidebars
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavItem {
    pub title: String,
    pub path: Option<String>,
    pub children: Vec<NavItem>,
}

/// A navigation entry flattened to a single list with a clamped indent level.
///
/// Sidebars render namespaces as a tree, but neither Askama templates nor the
/// nested-`<ul>` renderers cap how deep the indentation grows. Flattening to a
/// list lets every namespace render (no matter how deeply nested) while capping
/// the visual indentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlatNavItem {
    pub title: String,
    pub path: Option<String>,
    pub indent: u8,
}

/// Maximum visual indentation level for package namespace sidebars.
pub const PACKAGE_NAV_MAX_INDENT: u8 = 3;

/// Flatten a package nav tree into a depth-clamped list for the sidebar.
///
/// `collapse_title` mirrors the sidebars' behavior of collapsing the package
/// root (whose title matches the package) so its namespaces render at the top
/// level. `max_indent` caps how far entries are indented; deeper descendants
/// still render, just without additional indentation.
pub fn flatten_pkg_nav(nav: &[NavItem], collapse_title: &str, max_indent: u8) -> Vec<FlatNavItem> {
    fn push(item: &NavItem, depth: u8, max_indent: u8, out: &mut Vec<FlatNavItem>) {
        out.push(FlatNavItem {
            title: item.title.clone(),
            path: item.path.clone(),
            indent: depth.min(max_indent),
        });
        for child in &item.children {
            push(child, depth.saturating_add(1), max_indent, out);
        }
    }

    let mut out = Vec::new();
    for item in nav {
        let collapse_root = !collapse_title.is_empty()
            && (item.title == collapse_title || item.path.as_deref() == Some(collapse_title));
        if !collapse_root {
            push(item, 0, max_indent, &mut out);
        } else {
            for child in &item.children {
                push(child, 0, max_indent, &mut out);
            }
        }
    }
    out
}

/// A table of contents item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TocItem {
    pub name: String,
    pub anchor: String,
    #[serde(default)]
    pub is_core: bool,
    /// Indentation level (0 = h2, 1 = h3, etc.)
    #[serde(default)]
    pub indent: u8,
    /// Meta indicator badges (e.g., "schedule", "event", "webhook", "mcp")
    #[serde(default)]
    pub badges: Vec<String>,
}

/// A section in the table of contents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TocSection {
    pub title: String,
    pub items: Vec<TocItem>,
}

/// Type resolution information for linking
#[derive(Default)]
pub struct TypeRegistry {
    /// Types in the current namespace (just need anchor: #TypeName)
    pub current_ns: AHashSet<String>,
    /// Types in the same package but different namespace (need: /pkg/{pkg}/{ns}#TypeName)
    pub same_pkg: AHashMap<String, String>,
    /// Types in other packages (need: /pkg/{other_pkg}/{ns}#TypeName)
    pub cross_pkg: AHashMap<String, String>,
}

/// Result of rendering a package page
pub struct RenderedPkgPage {
    pub title: String,
    pub html: String,
    pub nav: Vec<NavItem>,
    pub toc: Vec<TocSection>,
}

/// Extract a short summary from a doc string (first line or sentence)
pub fn doc_summary(doc: &str) -> String {
    let first_line = doc.lines().next().unwrap_or(doc);
    if let Some(period_pos) = first_line.find(". ") {
        first_line[..=period_pos].to_string()
    } else {
        first_line.to_string()
    }
}

/// Render doc string as markdown to HTML
pub fn render_doc(doc: &str) -> String {
    markdown_to_html(doc)
}

/// Generate HTML for context variable requirements (namespace-level)
pub fn render_ctx_requirements(ctx: &CtxRequirements) -> String {
    let mut html = String::new();
    html.push_str("<div class=\"ctx-requirements\">\n");

    if !ctx.req.is_empty() {
        html.push_str("<div class=\"ctx-section\">\n");
        html.push_str("<span class=\"ctx-label ctx-required\">Required</span>\n");
        html.push_str("<ul class=\"ctx-list\">\n");
        for var in &ctx.req {
            html.push_str(&format!("<li><code>{}</code></li>\n", var));
        }
        html.push_str("</ul>\n</div>\n");
    }

    if !ctx.opt.is_empty() {
        html.push_str("<div class=\"ctx-section\">\n");
        html.push_str("<span class=\"ctx-label ctx-optional\">Optional</span>\n");
        html.push_str("<ul class=\"ctx-list\">\n");
        for var in &ctx.opt {
            html.push_str(&format!("<li><code>{}</code></li>\n", var));
        }
        html.push_str("</ul>\n</div>\n");
    }

    html.push_str("</div>\n");
    html
}

/// Generate inline HTML for function-level context variable requirements
pub fn render_fn_ctx_requirements(ctx: &CtxRequirements) -> String {
    let mut html = String::new();
    html.push_str("<div class=\"ctx-fn-requirements\">\n");
    html.push_str("<span class=\"ctx-fn-label\">Context Vars:</span> ");

    let mut parts = Vec::new();
    for var in &ctx.req {
        parts.push(format!("<code class=\"ctx-req\">{}</code>", var));
    }
    for var in &ctx.opt {
        parts.push(format!("<code class=\"ctx-opt\">{}</code>", var));
    }

    html.push_str(&parts.join(", "));
    html.push_str("\n</div>\n");
    html
}

/// Generate HTML for namespace-level container (box) requirements
pub fn render_box_requirements(box_req: &BoxRequirementDoc) -> String {
    let mut html = String::new();
    html.push_str("<div class=\"box-requirements\">\n");

    if let Some(ref size) = box_req.min_size {
        html.push_str("<div class=\"box-section\">\n");
        html.push_str("<span class=\"box-label box-size\">Minimum Size</span>\n");
        html.push_str(&format!("<code class=\"box-value\">{}</code>\n", size));
        html.push_str("</div>\n");
    }

    if box_req.network {
        html.push_str("<div class=\"box-section\">\n");
        html.push_str("<span class=\"box-label box-network\">Network</span>\n");
        html.push_str("<span class=\"box-value\">required</span>\n");
        html.push_str("</div>\n");
    }

    html.push_str("</div>\n");
    html
}

/// Generate inline HTML for function-level container (box) requirements
pub fn render_fn_box_requirements(box_req: &BoxRequirementDoc) -> String {
    let mut html = String::new();
    html.push_str("<div class=\"box-fn-requirements\">\n");
    html.push_str("<span class=\"box-fn-label\">Container:</span> ");

    let mut parts = Vec::new();
    if let Some(ref size) = box_req.min_size {
        parts.push(format!("<code class=\"box-fn-size\">{}</code>", size));
    }
    if box_req.network {
        parts.push("<code class=\"box-fn-network\">network</code>".to_string());
    }

    html.push_str(&parts.join(" + "));
    html.push_str("\n</div>\n");
    html
}

/// Extract namespace parts from a full namespace like "::hot::coll"
/// Returns (prefix, ns_name) e.g., ("::hot", "::coll")
pub fn parse_namespace(namespace: &str) -> (String, String) {
    let parts: Vec<&str> = namespace.split("::").filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2 {
        let prefix = format!("::{}", parts[..parts.len() - 1].join("::"));
        let ns_name = format!("::{}", parts[parts.len() - 1]);
        (prefix, ns_name)
    } else if parts.len() == 1 {
        (String::new(), format!("::{}", parts[0]))
    } else {
        (String::new(), namespace.to_string())
    }
}

#[derive(Default)]
struct NavNamespaceNode {
    title: String,
    path: Option<String>,
    children: BTreeMap<String, NavNamespaceNode>,
}

impl NavNamespaceNode {
    fn into_nav_item(self) -> NavItem {
        NavItem {
            title: self.title,
            path: self.path,
            children: self
                .children
                .into_values()
                .map(NavNamespaceNode::into_nav_item)
                .collect(),
        }
    }

    fn into_nav_items(self) -> Vec<NavItem> {
        self.children
            .into_values()
            .map(NavNamespaceNode::into_nav_item)
            .collect()
    }
}

fn namespace_parts(namespace: &str) -> Vec<&str> {
    namespace.split("::").filter(|s| !s.is_empty()).collect()
}

/// Generate navigation structure for package docs
///
/// The `base_path_prefix` is prepended to all paths:
/// - For registry docs: "hot.dev/anthropic" (org/pkg)
/// - For app docs: project-relative paths are handled separately
pub fn generate_pkg_nav(pkg_name: &str, docs: &PkgDocs) -> Vec<NavItem> {
    generate_pkg_nav_with_prefix(pkg_name, docs, pkg_name)
}

/// Generate navigation with a custom base path prefix
///
/// This allows callers to control the URL structure:
/// - registry docs: prefix = "hot.dev/anthropic" → paths like "hot.dev/anthropic/::anthropic::chat"
/// - app docs: prefix = "anthropic" → paths like "anthropic/::anthropic::chat" (rewritten by caller)
pub fn generate_pkg_nav_with_prefix(
    _pkg_name: &str,
    docs: &PkgDocs,
    base_path_prefix: &str,
) -> Vec<NavItem> {
    let mut root = NavNamespaceNode::default();
    for ns in &docs.namespaces {
        let parts = namespace_parts(&ns.namespace);
        if parts.is_empty() {
            continue;
        }

        let mut node = &mut root;
        for part in &parts {
            node = node
                .children
                .entry((*part).to_string())
                .or_insert_with(|| NavNamespaceNode {
                    title: format!("::{}", part),
                    path: None,
                    children: BTreeMap::new(),
                });
        }

        node.path = Some(format!("{}/{}", base_path_prefix, ns.name));
    }

    vec![NavItem {
        title: docs.meta.name.clone(),
        path: Some(base_path_prefix.to_string()),
        children: root.into_nav_items(),
    }]
}

/// Generate HTML content for package index page
/// `base_url` is the URL prefix for links (e.g., "/pkg" for registry docs, "/docs/{project}/pkg" for app docs)
/// `pkg_name` is the full package path (e.g., "hot.dev/hot-std")
pub fn generate_pkg_index_html(docs: &PkgDocs, pkg_name: &str, base_url: &str) -> String {
    let mut html = String::new();

    // Title with description
    if !docs.meta.description.is_empty() {
        html.push_str(&format!(
            "<h1>{} <span class=\"pkg-description\">— {}</span></h1>\n",
            docs.meta.name, docs.meta.description
        ));
    } else {
        html.push_str(&format!("<h1>{}</h1>\n", docs.meta.name));
    }

    // Version and metadata
    html.push_str("<div class=\"pkg-meta\">\n");
    html.push_str(&format!(
        "<p><strong>Version:</strong> {}</p>\n",
        docs.meta.version
    ));
    if !docs.meta.author.is_empty() {
        html.push_str(&format!(
            "<p><strong>Author:</strong> {}</p>\n",
            docs.meta.author
        ));
    }
    if !docs.meta.license.is_empty() {
        html.push_str(&format!(
            "<p><strong>License:</strong> {}</p>\n",
            docs.meta.license
        ));
    }

    // Dependencies section
    if !docs.meta.deps.is_empty() {
        html.push_str("<p><strong>Dependencies:</strong> ");
        let dep_links: Vec<String> = docs
            .meta
            .deps
            .iter()
            .map(|dep| {
                if let Some(ref docs_url) = dep.docs_url {
                    format!("<a href=\"{}\">{}</a>", docs_url, dep.name)
                } else if let Some(ref git) = dep.git {
                    let mut url = git.clone();
                    if url.starts_with("git@github.com:") {
                        url = url
                            .replace("git@github.com:", "https://github.com/")
                            .trim_end_matches(".git")
                            .to_string();
                    }
                    if let Some(ref path) = dep.path {
                        url = format!("{}/tree/main/{}", url.trim_end_matches(".git"), path);
                    }
                    format!("<a href=\"{}\" target=\"_blank\">{}</a>", url, dep.name)
                } else {
                    dep.name.clone()
                }
            })
            .collect();
        html.push_str(&dep_links.join(", "));
        html.push_str("</p>\n");
    }

    html.push_str("</div>\n\n");

    // Add to your project section (skip for hot-std since it's bundled with the CLI)
    // pkg_name is now the full path like "hot.dev/twilio" or "hot.dev/hot-std"
    let is_hot_std = pkg_name == "hot-std" || pkg_name.ends_with("/hot-std");
    if !is_hot_std {
        html.push_str(&format!(r#"<div class="add-to-project">
  <div class="add-to-project-header">
    <h3>
      <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 10v6m0 0l-3-3m3 3l3-3m2 8H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z"/>
      </svg>
      Add to your project
    </h3>
    <button onclick="copyDepsToClipboard()" class="copy-btn" title="Copy to clipboard">
      <svg class="copy-icon" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z"/>
      </svg>
      <svg class="check-icon hidden" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7"/>
      </svg>
      <span class="copy-text">Copy</span>
    </button>
  </div>
  <pre class="deps-code-block"><code><span class="dep-string">"{0}"</span>: <span class="dep-string">"{1}"</span></code></pre>
  <p class="add-to-project-note">Add this to your <code>deps</code> in <code>hot.hot</code></p>
</div>
"#, pkg_name, docs.meta.version));
    }

    // Namespace list grouped by prefix
    html.push_str("<h2>Namespaces</h2>\n");

    let mut groups: BTreeMap<String, Vec<&DocNamespace>> = BTreeMap::new();
    for ns in &docs.namespaces {
        let (prefix, _) = parse_namespace(&ns.namespace);
        groups.entry(prefix).or_default().push(ns);
    }

    for (prefix, namespaces) in groups {
        if !prefix.is_empty() {
            html.push_str(&format!("<h3 class=\"namespace-header\">{}</h3>\n", prefix));
        }

        // Determine which meta columns have any data across this group
        let has_schedules = namespaces
            .iter()
            .any(|ns| ns.functions.iter().any(|f| f.schedule.is_some()));
        let has_events = namespaces
            .iter()
            .any(|ns| ns.functions.iter().any(|f| f.on_event.is_some()));
        let has_webhooks = namespaces
            .iter()
            .any(|ns| ns.functions.iter().any(|f| f.webhook.is_some()));
        let has_mcp = namespaces
            .iter()
            .any(|ns| ns.functions.iter().any(|f| f.mcp.is_some()));
        let has_sends = namespaces
            .iter()
            .any(|ns| ns.functions.iter().any(|f| !f.sends.is_empty()));

        html.push_str("<table>\n<thead><tr><th>Namespace</th><th>Functions</th><th>Types</th>");
        if has_schedules {
            html.push_str("<th>Schedules</th>");
        }
        if has_events {
            html.push_str("<th>Events</th>");
        }
        if has_webhooks {
            html.push_str("<th>Webhooks</th>");
        }
        if has_mcp {
            html.push_str("<th>MCP</th>");
        }
        if has_sends {
            html.push_str("<th>Sends</th>");
        }
        html.push_str("<th>Description</th></tr></thead>\n<tbody>\n");

        for ns in namespaces {
            let (_, ns_display) = parse_namespace(&ns.namespace);
            let fn_count = ns.functions.len();
            let type_count = ns.types.len();

            let desc = ns
                .doc
                .as_ref()
                .or_else(|| ns.functions.first().and_then(|f| f.doc.as_ref()))
                .map(|d| doc_summary(d))
                .unwrap_or_default();

            let type_cell = if type_count > 0 {
                type_count.to_string()
            } else {
                String::from("—")
            };

            html.push_str(&format!(
                "<tr><td><code class=\"namespace-badge\"><a href=\"{}/{}/{}\">{}</a></code></td><td>{}</td><td>{}</td>",
                base_url, pkg_name, ns.name, ns_display, fn_count, type_cell
            ));

            if has_schedules {
                let c = ns.functions.iter().filter(|f| f.schedule.is_some()).count();
                html.push_str(&format!(
                    "<td>{}</td>",
                    if c > 0 {
                        c.to_string()
                    } else {
                        "—".to_string()
                    }
                ));
            }
            if has_events {
                let c = ns.functions.iter().filter(|f| f.on_event.is_some()).count();
                html.push_str(&format!(
                    "<td>{}</td>",
                    if c > 0 {
                        c.to_string()
                    } else {
                        "—".to_string()
                    }
                ));
            }
            if has_webhooks {
                let c = ns.functions.iter().filter(|f| f.webhook.is_some()).count();
                html.push_str(&format!(
                    "<td>{}</td>",
                    if c > 0 {
                        c.to_string()
                    } else {
                        "—".to_string()
                    }
                ));
            }
            if has_mcp {
                let c = ns.functions.iter().filter(|f| f.mcp.is_some()).count();
                html.push_str(&format!(
                    "<td>{}</td>",
                    if c > 0 {
                        c.to_string()
                    } else {
                        "—".to_string()
                    }
                ));
            }
            if has_sends {
                let c = ns.functions.iter().filter(|f| !f.sends.is_empty()).count();
                html.push_str(&format!(
                    "<td>{}</td>",
                    if c > 0 {
                        c.to_string()
                    } else {
                        "—".to_string()
                    }
                ));
            }

            html.push_str(&format!("<td>{}</td></tr>\n", desc));
        }
        html.push_str("</tbody></table>\n");
    }

    html
}

/// Generate a JSON type registry for client-side type linking
fn generate_type_registry_json(registry: &TypeRegistry) -> String {
    let mut map = AHashMap::new();

    for type_name in &registry.current_ns {
        map.insert(type_name.clone(), format!("#{}", type_name));
    }

    // same_pkg values already contain the full URL path (including base_url)
    for (type_name, full_path) in &registry.same_pkg {
        map.insert(type_name.clone(), format!("{}#{}", full_path, type_name));
    }

    for (type_name, full_path) in &registry.cross_pkg {
        map.insert(type_name.clone(), full_path.clone());
    }

    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

/// Build links for documented vars that aliases can point to.
pub fn build_alias_target_index(
    docs: &PkgDocs,
    pkg_name: &str,
    base_url: &str,
) -> AHashMap<String, String> {
    let mut index = AHashMap::new();
    extend_alias_target_index(&mut index, docs, pkg_name, base_url);
    index
}

/// Add documented vars from a package to an existing alias target link index.
pub fn extend_alias_target_index(
    index: &mut AHashMap<String, String>,
    docs: &PkgDocs,
    pkg_name: &str,
    base_url: &str,
) {
    for ns in &docs.namespaces {
        let ns_url = format!("{}/{}/{}", base_url, pkg_name, ns.name);
        for func in &ns.functions {
            index.insert(
                format!("{}/{}", ns.namespace, func.name),
                format!("{}#{}", ns_url, func.name),
            );
        }
        for typ in &ns.types {
            index.insert(
                format!("{}/{}", ns.namespace, typ.name),
                format!("{}#{}", ns_url, typ.name),
            );
        }
        for value in &ns.values {
            index.insert(
                format!("{}/{}", ns.namespace, value.name),
                format!("{}#{}", ns_url, value.name),
            );
        }
    }
}

fn render_alias_badge(
    html: &mut String,
    alias_of: &Option<String>,
    alias_target_index: &AHashMap<String, String>,
) {
    if let Some(target) = alias_of {
        let title = format!("Alias of {}", target);
        if let Some(href) = alias_target_index.get(target) {
            html.push_str(&format!(
                " <span class=\"meta-badge alias-badge\">alias</span> <span class=\"alias-target\"><span class=\"alias-target-label\">of</span> <a href=\"{}\" title=\"{}\"><code>{}</code></a></span>",
                html_escape(href),
                html_escape(&title),
                html_escape(target)
            ));
        } else {
            html.push_str(&format!(
                " <span class=\"meta-badge alias-badge\">alias</span> <span class=\"alias-target\"><span class=\"alias-target-label\">of</span> <code title=\"{}\">{}</code></span>",
                html_escape(&title),
                html_escape(target)
            ));
        }
    }
}

/// Generate HTML content for a namespace page
pub fn generate_namespace_html(ns: &DocNamespace, pkg_name: &str) -> String {
    generate_namespace_html_with_registry(ns, pkg_name, &AHashMap::new(), None, "/pkg")
}

/// Generate HTML content for a namespace page with full type registry support
/// `base_url` is the URL prefix for links (e.g., "/pkg" for registry docs, "/docs/{project}/pkg" for app docs)
pub fn generate_namespace_html_with_registry(
    ns: &DocNamespace,
    pkg_name: &str,
    pkg_type_index: &AHashMap<String, String>,
    cross_pkg_registry: Option<&AHashMap<String, String>>,
    base_url: &str,
) -> String {
    generate_namespace_html_with_registry_and_aliases(
        ns,
        pkg_name,
        pkg_type_index,
        cross_pkg_registry,
        base_url,
        &AHashMap::new(),
    )
}

/// Generate HTML content for a namespace page with type and alias link support.
pub fn generate_namespace_html_with_registry_and_aliases(
    ns: &DocNamespace,
    pkg_name: &str,
    pkg_type_index: &AHashMap<String, String>,
    cross_pkg_registry: Option<&AHashMap<String, String>>,
    base_url: &str,
    alias_target_index: &AHashMap<String, String>,
) -> String {
    let mut html = String::new();

    // Build the type registry for this namespace
    let mut registry = TypeRegistry::default();

    for typ in &ns.types {
        registry.current_ns.insert(typ.name.clone());
    }

    for (type_name, ns_path) in pkg_type_index {
        if !registry.current_ns.contains(type_name) {
            registry.same_pkg.insert(
                type_name.clone(),
                format!("{}/{}/{}", base_url, pkg_name, ns_path),
            );
        }
    }

    if let Some(cross_pkg) = cross_pkg_registry {
        for (type_name, full_path) in cross_pkg {
            if !registry.current_ns.contains(type_name)
                && !registry.same_pkg.contains_key(type_name)
            {
                registry
                    .cross_pkg
                    .insert(type_name.clone(), full_path.clone());
            }
        }
    }

    // Breadcrumb
    html.push_str("<p class=\"breadcrumb\">");
    html.push_str(&format!(
        "<a href=\"{}/{}\">{}</a> / ",
        base_url, pkg_name, pkg_name
    ));
    html.push_str(&format!(
        "<span class=\"ns-current\">{}</span>",
        ns.namespace
    ));
    html.push_str("</p>\n\n");

    if let Some(doc) = &ns.doc {
        html.push_str(&render_doc(doc));
    }

    // Context Vars section
    if let Some(ctx) = &ns.ctx {
        html.push_str("<h2 id=\"context-vars\">Context Vars</h2>\n");
        html.push_str(&render_ctx_requirements(ctx));
    }

    // Container Requirements section
    if let Some(box_req) = &ns.box_req {
        html.push_str("<h2 id=\"container-requirements\">Container Requirements</h2>\n");
        html.push_str(&render_box_requirements(box_req));
    }

    // Values section (top-level literals/constants)
    if !ns.values.is_empty() {
        html.push_str("<h2>Values</h2>\n");

        let mut all_values: Vec<_> = ns.values.iter().collect();
        all_values.sort_by(|a, b| a.name.cmp(&b.name));

        for value in all_values {
            html.push_str(&format!(
                "<h3 id=\"{}\"><code>{}</code>",
                value.name,
                html_escape(&value.name)
            ));
            render_alias_badge(&mut html, &value.alias_of, alias_target_index);
            html.push_str("</h3>\n");
            html.push_str("<pre><code class=\"language-hot\">");
            html.push_str(&html_escape(&value.name));
            if let Some(type_annotation) = &value.type_annotation {
                html.push_str(": ");
                html.push_str(&html_escape(type_annotation));
            }
            html.push(' ');
            html.push_str(&html_escape(&value.value));
            html.push_str("</code></pre>\n");

            if let Some(doc) = &value.doc {
                html.push_str(&render_doc(doc));
            }

            html.push('\n');
        }
    }

    // Functions section (listed before types — functions are the primary API)
    if !ns.functions.is_empty() {
        html.push_str("<h2>Functions</h2>\n");

        let mut all_funcs: Vec<_> = ns.functions.iter().collect();
        all_funcs.sort_by(|a, b| match (b.is_core, a.is_core) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => a.name.cmp(&b.name),
        });

        for func in all_funcs {
            html.push_str(&format!(
                "<h3 id=\"{}\"><code>{}</code>",
                func.name, func.name
            ));
            if func.is_core {
                html.push_str(" <span class=\"core-badge\">core</span>");
            }
            if let Some(cron) = &func.schedule {
                html.push_str(&format!(
                    " <span class=\"meta-badge schedule-badge\" title=\"Schedule: {}\">schedule</span>",
                    html_escape(cron)
                ));
            }
            if let Some(event) = &func.on_event {
                html.push_str(&format!(
                    " <span class=\"meta-badge event-badge\" title=\"Event: {}\">event</span>",
                    html_escape(event)
                ));
            }
            if let Some(wh) = &func.webhook {
                let title = if wh.path.is_empty() {
                    format!("Webhook: {}", html_escape(&wh.service))
                } else {
                    format!(
                        "Webhook: {} {}",
                        html_escape(&wh.service),
                        html_escape(&wh.path)
                    )
                };
                html.push_str(&format!(
                    " <span class=\"meta-badge webhook-badge\" title=\"{}\">webhook</span>",
                    title
                ));
            }
            if let Some(mcp) = &func.mcp {
                let title = if mcp.name.is_empty() {
                    format!("MCP: {}", html_escape(&mcp.service))
                } else {
                    format!(
                        "MCP: {} / {}",
                        html_escape(&mcp.service),
                        html_escape(&mcp.name)
                    )
                };
                html.push_str(&format!(
                    " <span class=\"meta-badge mcp-badge\" title=\"{}\">mcp</span>",
                    title
                ));
            }
            if !func.sends.is_empty() {
                let title = format!("Sends: {}", func.sends.join(", "));
                html.push_str(&format!(
                    " <span class=\"meta-badge sends-badge\" title=\"{}\">sends</span>",
                    html_escape(&title)
                ));
            }
            render_alias_badge(&mut html, &func.alias_of, alias_target_index);
            html.push_str("</h3>\n");

            if !func.signatures.is_empty() {
                html.push_str("<pre class=\"fn-signature\"><code class=\"language-hot\">");
                for (i, sig) in func.signatures.iter().enumerate() {
                    if i > 0 {
                        html.push('\n');
                    }
                    html.push_str("fn (");
                    for (j, param) in sig.params.iter().enumerate() {
                        if j > 0 {
                            html.push_str(", ");
                        }
                        if param.is_lazy {
                            html.push_str("lazy ");
                        }
                        if param.is_variadic {
                            html.push_str("...");
                        }
                        html.push_str(&param.name);
                        if let Some(type_ann) = &param.type_annotation {
                            html.push_str(": ");
                            html.push_str(type_ann);
                        }
                    }
                    html.push(')');
                    if let Some(ret) = &sig.return_type {
                        html.push_str(": ");
                        html.push_str(ret);
                    }
                }
                html.push_str("</code></pre>\n");
            }

            if let Some(doc) = &func.doc {
                html.push_str(&render_doc(doc));
            }

            if let Some(ctx) = &func.ctx {
                html.push_str(&render_fn_ctx_requirements(ctx));
            }

            if let Some(box_req) = &func.box_req {
                html.push_str(&render_fn_box_requirements(box_req));
            }

            // Render meta details block
            let has_meta = func.schedule.is_some()
                || func.on_event.is_some()
                || func.webhook.is_some()
                || func.mcp.is_some()
                || !func.sends.is_empty();
            if has_meta {
                html.push_str("<div class=\"meta-details\">\n");
                if let Some(cron) = &func.schedule {
                    html.push_str(&format!(
                        "<div class=\"meta-detail\"><span class=\"meta-badge schedule-badge\">schedule</span> <code>{}</code></div>\n",
                        html_escape(cron)
                    ));
                }
                if let Some(event) = &func.on_event {
                    html.push_str(&format!(
                        "<div class=\"meta-detail\"><span class=\"meta-badge event-badge\">event</span> <code>{}</code></div>\n",
                        html_escape(event)
                    ));
                }
                if let Some(wh) = &func.webhook {
                    let detail = if wh.path.is_empty() {
                        format!("service: {}", html_escape(&wh.service))
                    } else {
                        format!(
                            "service: {}, path: {}",
                            html_escape(&wh.service),
                            html_escape(&wh.path)
                        )
                    };
                    html.push_str(&format!(
                        "<div class=\"meta-detail\"><span class=\"meta-badge webhook-badge\">webhook</span> <code>{}</code></div>\n",
                        detail
                    ));
                }
                if let Some(mcp) = &func.mcp {
                    let detail = if mcp.name.is_empty() {
                        format!("service: {}", html_escape(&mcp.service))
                    } else {
                        format!(
                            "service: {}, name: {}",
                            html_escape(&mcp.service),
                            html_escape(&mcp.name)
                        )
                    };
                    html.push_str(&format!(
                        "<div class=\"meta-detail\"><span class=\"meta-badge mcp-badge\">mcp</span> <code>{}</code></div>\n",
                        detail
                    ));
                }
                if !func.sends.is_empty() {
                    let events: Vec<String> = func
                        .sends
                        .iter()
                        .map(|s| format!("<code>{}</code>", html_escape(s)))
                        .collect();
                    html.push_str(&format!(
                        "<div class=\"meta-detail\"><span class=\"meta-badge sends-badge\">sends</span> {}</div>\n",
                        events.join(", ")
                    ));
                }
                html.push_str("</div>\n");
            }

            html.push('\n');
        }
    }

    // Types section (after functions — types are supporting reference)
    if !ns.types.is_empty() {
        html.push_str("<h2>Types</h2>\n");

        let mut all_types: Vec<_> = ns.types.iter().collect();
        all_types.sort_by(|a, b| match (b.is_core, a.is_core) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => a.name.cmp(&b.name),
        });

        for typ in all_types {
            html.push_str(&format!(
                "<h3 id=\"{}\"><code>{}</code>",
                typ.name, typ.name
            ));
            if typ.is_core {
                html.push_str(" <span class=\"core-badge\">core</span>");
            }
            render_alias_badge(&mut html, &typ.alias_of, alias_target_index);
            html.push_str("</h3>\n");

            if !typ.constructors.is_empty() {
                html.push_str("<pre class=\"fn-signature\"><code class=\"language-hot\">");
                for (i, ctor) in typ.constructors.iter().enumerate() {
                    if i > 0 {
                        html.push('\n');
                    }
                    html.push_str("fn (");
                    for (j, param) in ctor.params.iter().enumerate() {
                        if j > 0 {
                            html.push_str(", ");
                        }
                        html.push_str(&param.name);
                        if let Some(type_ann) = &param.type_annotation {
                            html.push_str(": ");
                            html.push_str(type_ann);
                        }
                    }
                    html.push(')');
                    if let Some(ret) = &ctor.return_type {
                        html.push_str(": ");
                        html.push_str(ret);
                    }
                }
                html.push_str("</code></pre>\n");
            }

            if let Some(type_alias) = &typ.type_alias {
                html.push_str("<pre><code class=\"language-hot\">");
                html.push_str(&format!(
                    "{} type {}",
                    html_escape(&typ.name),
                    html_escape(type_alias)
                ));
                html.push_str("</code></pre>\n");
            } else if !typ.fields.is_empty() {
                html.push_str("<pre><code class=\"language-hot\">");
                html.push_str(&format!("{} type {{\n", typ.name));
                let field_count = typ.fields.len();
                for (i, field) in typ.fields.iter().enumerate() {
                    let type_ann = field
                        .type_annotation
                        .as_ref()
                        .map(|t| format!(": {}", t))
                        .unwrap_or_default();
                    let comma = if i < field_count - 1 { "," } else { "" };
                    html.push_str(&format!("    {}{}{}\n", field.name, type_ann, comma));
                }
                html.push_str("}</code></pre>\n");
            }

            if let Some(doc) = &typ.doc {
                html.push_str(&render_doc(doc));
            }

            html.push('\n');
        }
    }

    // Output the type registry as JSON for client-side linking
    let registry_json = generate_type_registry_json(&registry);
    html.push_str(&format!(
        "<script type=\"application/json\" id=\"type-registry-data\">{}</script>\n",
        registry_json
    ));

    html
}

/// Build TOC for a namespace page
pub fn build_namespace_toc(ns: &DocNamespace) -> Vec<TocSection> {
    let mut toc = Vec::new();

    if ns.ctx.is_some() {
        toc.push(TocSection {
            title: "Context Vars".to_string(),
            items: vec![TocItem {
                name: "Context Vars".to_string(),
                anchor: "context-vars".to_string(),
                is_core: false,
                indent: 0,
                badges: vec![],
            }],
        });
    }

    if ns.box_req.is_some() {
        toc.push(TocSection {
            title: "Container Requirements".to_string(),
            items: vec![TocItem {
                name: "Container Requirements".to_string(),
                anchor: "container-requirements".to_string(),
                is_core: false,
                indent: 0,
                badges: vec![],
            }],
        });
    }

    if !ns.values.is_empty() {
        let mut values: Vec<_> = ns.values.iter().collect();
        values.sort_by(|a, b| a.name.cmp(&b.name));

        let items: Vec<TocItem> = values
            .iter()
            .map(|value| TocItem {
                name: value.name.clone(),
                anchor: value.name.clone(),
                is_core: false,
                indent: 0,
                badges: vec![],
            })
            .collect();

        toc.push(TocSection {
            title: "Values".to_string(),
            items,
        });
    }

    if !ns.functions.is_empty() {
        let mut funcs: Vec<_> = ns.functions.iter().collect();
        funcs.sort_by(|a, b| match (b.is_core, a.is_core) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => a.name.cmp(&b.name),
        });

        let items: Vec<TocItem> = funcs
            .iter()
            .map(|f| {
                let mut badges = Vec::new();
                if f.schedule.is_some() {
                    badges.push("schedule".to_string());
                }
                if f.on_event.is_some() {
                    badges.push("event".to_string());
                }
                if f.webhook.is_some() {
                    badges.push("webhook".to_string());
                }
                if f.mcp.is_some() {
                    badges.push("mcp".to_string());
                }
                if !f.sends.is_empty() {
                    badges.push("sends".to_string());
                }
                TocItem {
                    name: f.name.clone(),
                    anchor: f.name.clone(),
                    is_core: f.is_core,
                    indent: 0,
                    badges,
                }
            })
            .collect();
        toc.push(TocSection {
            title: "Functions".to_string(),
            items,
        });
    }

    if !ns.types.is_empty() {
        let items: Vec<TocItem> = ns
            .types
            .iter()
            .map(|t| TocItem {
                name: t.name.clone(),
                anchor: t.name.clone(),
                is_core: t.is_core,
                indent: 0,
                badges: vec![],
            })
            .collect();
        toc.push(TocSection {
            title: "Types".to_string(),
            items,
        });
    }

    toc
}

/// Build TOC for package index page
pub fn build_pkg_index_toc(docs: &PkgDocs) -> Vec<TocSection> {
    let mut toc = Vec::new();

    // Group namespaces by prefix
    let mut groups: std::collections::BTreeMap<String, Vec<&DocNamespace>> =
        std::collections::BTreeMap::new();
    for ns in &docs.namespaces {
        let (prefix, _) = parse_namespace(&ns.namespace);
        groups.entry(prefix).or_default().push(ns);
    }

    // Add each namespace group as a section
    for (prefix, namespaces) in groups {
        let section_title = if prefix.is_empty() {
            "Namespaces".to_string()
        } else {
            prefix
        };

        let items: Vec<TocItem> = namespaces
            .iter()
            .map(|ns| {
                let (_, ns_display) = parse_namespace(&ns.namespace);
                TocItem {
                    name: ns_display,
                    anchor: format!("ns-{}", ns.name.replace("::", "-").replace("/", "-")),
                    is_core: false,
                    indent: 0,
                    badges: vec![],
                }
            })
            .collect();

        toc.push(TocSection {
            title: section_title,
            items,
        });
    }

    toc
}

/// Render a full package page (index or namespace)
/// `base_url` is the URL prefix for links (e.g., "/pkg" for registry docs)
pub fn render_pkg_page(
    pkg_docs: &PkgDocs,
    pkg_name: &str,
    module_path: &str,
    cross_pkg_registry: Option<&AHashMap<String, String>>,
    base_url: &str,
) -> Result<RenderedPkgPage, String> {
    let nav = generate_pkg_nav(pkg_name, pkg_docs);

    if module_path.is_empty() || module_path == "index" {
        let html = generate_pkg_index_html(pkg_docs, pkg_name, base_url);
        let title = format!("{} - Package Documentation", pkg_docs.meta.name);
        Ok(RenderedPkgPage {
            title,
            html,
            nav,
            toc: Vec::new(),
        })
    } else {
        let namespace = pkg_docs
            .namespaces
            .iter()
            .find(|m| m.name == module_path)
            .ok_or_else(|| {
                format!(
                    "Namespace '{}' not found in package '{}'",
                    module_path, pkg_name
                )
            })?;

        let alias_target_index = build_alias_target_index(pkg_docs, pkg_name, base_url);
        let html = generate_namespace_html_with_registry_and_aliases(
            namespace,
            pkg_name,
            &pkg_docs.type_index,
            cross_pkg_registry,
            base_url,
            &alias_target_index,
        );
        let (_, ns_name) = parse_namespace(&namespace.namespace);
        let title = format!(
            "{} - {} - Package Documentation",
            ns_name, pkg_docs.meta.name
        );
        let toc = build_namespace_toc(namespace);

        Ok(RenderedPkgPage {
            title,
            html,
            nav,
            toc,
        })
    }
}

/// Get previous and next pages for navigation
pub fn get_prev_next(nav: &[NavItem], current_path: &str) -> (Option<NavItem>, Option<NavItem>) {
    fn flatten(items: &[NavItem]) -> Vec<NavItem> {
        let mut result = Vec::new();
        for item in items {
            if item.path.is_some() {
                result.push(item.clone());
            }
            result.extend(flatten(&item.children));
        }
        result
    }

    let flat = flatten(nav);
    let current_idx = flat
        .iter()
        .position(|item| item.path.as_deref() == Some(current_path));

    match current_idx {
        Some(idx) => {
            let prev = if idx > 0 {
                Some(flat[idx - 1].clone())
            } else {
                None
            };
            let next = if idx < flat.len() - 1 {
                Some(flat[idx + 1].clone())
            } else {
                None
            };
            (prev, next)
        }
        None => (None, None),
    }
}

/// Find the current nav item and its ancestor titles (for breadcrumbs/highlighting).
///
/// Returns the chain of titles from the root down to the item whose `path`
/// matches `target_path`, or an empty vec if no item matches.
pub fn find_nav_path(nav: &[NavItem], target_path: &str) -> Vec<String> {
    fn find_recursive(items: &[NavItem], target: &str, path: &mut Vec<String>) -> bool {
        for item in items {
            if let Some(ref item_path) = item.path
                && item_path == target
            {
                path.push(item.title.clone());
                return true;
            }
            if !item.children.is_empty() {
                path.push(item.title.clone());
                if find_recursive(&item.children, target, path) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }

    let mut path = Vec::new();
    find_recursive(nav, target_path, &mut path);
    path
}

/// Convert markdown to HTML with code block language annotations and heading IDs
pub fn markdown_to_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let parser = Parser::new_ext(markdown, options);
    let events: Vec<Event> = parser.collect();

    let mut html_output = String::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_content = String::new();
    let mut in_heading = false;
    let mut heading_level = 0u8;
    let mut heading_text = String::new();

    for event in events {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                heading_level = level as u8;
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                let id = slugify(&heading_text);
                html_output.push_str(&format!(
                    "<h{} id=\"{}\">{}</h{}>\n",
                    heading_level,
                    id,
                    html_escape(&heading_text),
                    heading_level
                ));
            }
            Event::Text(text) if in_heading => {
                heading_text.push_str(&text);
            }
            Event::Code(text) if in_heading => {
                heading_text.push_str(&text);
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
                in_code_block = true;
                code_lang = lang.to_string();
                code_content.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;

                let code_lang = code_lang
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim();

                if code_lang == "result" {
                    html_output.push_str(&format!(
                        "<div class=\"result-block\"><pre><code class=\"language-plaintext\">{}</code></pre></div>\n",
                        html_escape(&code_content)
                    ));
                } else {
                    let lang_class = if code_lang.is_empty() {
                        if looks_like_hot_code_block(&code_content) {
                            "language-hot".to_string()
                        } else {
                            "language-plaintext".to_string()
                        }
                    } else {
                        format!("language-{}", code_lang)
                    };
                    html_output.push_str(&format!(
                        "<pre><code class=\"{}\">{}</code></pre>\n",
                        html_escape(&lang_class),
                        html_escape(&code_content)
                    ));
                }
            }
            Event::Text(text) if in_code_block => {
                code_content.push_str(&text);
            }
            _ => {
                if !in_code_block && !in_heading {
                    let mut single_html = String::new();
                    html::push_html(&mut single_html, std::iter::once(event));
                    html_output.push_str(&single_html);
                }
            }
        }
    }

    let mut sanitizer = ammonia::Builder::default();
    sanitizer
        .add_tag_attributes("a", &["class", "target"])
        .add_tag_attributes("code", &["class"])
        .add_tag_attributes("div", &["class"])
        .add_tag_attributes("pre", &["class"])
        .add_tag_attributes("span", &["class"])
        .add_tag_attributes("h1", &["id"])
        .add_tag_attributes("h2", &["id"])
        .add_tag_attributes("h3", &["id"])
        .add_tag_attributes("h4", &["id"])
        .add_tag_attributes("h5", &["id"])
        .add_tag_attributes("h6", &["id"]);
    sanitizer.clean(&html_output).to_string()
}

fn looks_like_hot_code_block(code: &str) -> bool {
    let lines: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect();

    if lines.is_empty() {
        return false;
    }

    let first = lines[0];

    if first.starts_with("::")
        || first.contains(" fn ")
        || first.contains(" type ")
        || first.contains(" enum ")
        || first.contains(" -> ")
    {
        return true;
    }

    if first.starts_with('(') && first.contains(':') {
        return true;
    }

    first.ends_with('(')
        && lines.iter().take(8).any(|line| line.contains(':'))
        && lines
            .iter()
            .take(12)
            .any(|line| line.starts_with("):") || line.starts_with(") ->"))
}

/// HTML escape for code content
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Convert a heading to a URL-friendly slug
fn slugify(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkg::docs::{
        BoxRequirementDoc, CtxRequirements, DocFunction, DocNamespace, DocType, DocValue, PkgMeta,
    };

    #[test]
    fn flatten_pkg_nav_clamps_indent_but_keeps_deep_entries() {
        let nav = vec![NavItem {
            title: "anthropic".to_string(),
            path: Some("hot.dev/anthropic".to_string()),
            children: vec![NavItem {
                title: "::anthropic".to_string(),
                path: Some("hot.dev/anthropic/::anthropic".to_string()),
                children: vec![NavItem {
                    title: "::api".to_string(),
                    path: Some("hot.dev/anthropic/::anthropic::api".to_string()),
                    children: vec![NavItem {
                        title: "::v1".to_string(),
                        path: Some("hot.dev/anthropic/::anthropic::api::v1".to_string()),
                        children: vec![NavItem {
                            title: "::beta".to_string(),
                            path: Some("hot.dev/anthropic/::anthropic::api::v1::beta".to_string()),
                            children: vec![NavItem {
                                title: "::messages".to_string(),
                                path: Some(
                                    "hot.dev/anthropic/::anthropic::api::v1::beta::messages"
                                        .to_string(),
                                ),
                                children: vec![],
                            }],
                        }],
                    }],
                }],
            }],
        }];

        let flat = flatten_pkg_nav(&nav, "anthropic", PACKAGE_NAV_MAX_INDENT);

        // Package root collapsed; every descendant still present.
        let titles: Vec<&str> = flat.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["::anthropic", "::api", "::v1", "::beta", "::messages"]
        );

        // Indentation increases through the configured max indent, then clamps.
        let indents: Vec<u8> = flat.iter().map(|item| item.indent).collect();
        assert_eq!(indents, vec![0, 1, 2, 3, 3]);
    }

    #[test]
    fn flatten_pkg_nav_keeps_root_when_title_differs() {
        let nav = vec![NavItem {
            title: "Documentation".to_string(),
            path: None,
            children: vec![NavItem {
                title: "Guide".to_string(),
                path: Some("guide".to_string()),
                children: vec![],
            }],
        }];

        let flat = flatten_pkg_nav(&nav, "anthropic", PACKAGE_NAV_MAX_INDENT);
        let rows: Vec<(&str, u8)> = flat
            .iter()
            .map(|item| (item.title.as_str(), item.indent))
            .collect();
        assert_eq!(rows, vec![("Documentation", 0), ("Guide", 1)]);
    }

    #[test]
    fn flatten_pkg_nav_collapses_root_by_path() {
        let nav = vec![NavItem {
            title: "hot.dev/anthropic".to_string(),
            path: Some("hot.dev/anthropic".to_string()),
            children: vec![NavItem {
                title: "::anthropic".to_string(),
                path: Some("hot.dev/anthropic/::anthropic".to_string()),
                children: vec![],
            }],
        }];

        let flat = flatten_pkg_nav(&nav, "hot.dev/anthropic", PACKAGE_NAV_MAX_INDENT);

        let rows: Vec<(&str, u8)> = flat
            .iter()
            .map(|item| (item.title.as_str(), item.indent))
            .collect();
        assert_eq!(rows, vec![("::anthropic", 0)]);
    }

    #[test]
    fn bare_hot_signature_fence_infers_hot_language() {
        let html = markdown_to_html(
            r#"
```
chat-with-tools(
    model: Str,
    messages: Vec<::ai::chat/Message>,
    system: Str?,
    tools: Vec<::ai::tool/Tool>?
): ::ai::chat/ChatReply
```
"#,
        );

        assert!(html.contains(r#"class="language-hot""#));
    }

    #[test]
    fn bare_non_hot_fence_stays_plaintext() {
        let html = markdown_to_html(
            r#"
```
[total_length:4][headers_length:4][prelude_crc:4][headers:*]
```
"#,
        );

        assert!(html.contains(r#"class="language-plaintext""#));
        assert!(!html.contains(r#"class="language-hot""#));
    }

    #[test]
    fn markdown_sanitizer_removes_active_content_and_unsafe_links() {
        let html = markdown_to_html(
            r#"
<script>alert(1)</script>
<img src=x onerror=alert(2)>
[unsafe](javascript:alert(3))

| A | B |
|---|---|
| 1 | 2 |
"#,
        );

        assert!(!html.contains("<script"));
        assert!(!html.contains("onerror"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("<table>"));
    }

    #[test]
    fn namespace_doc_renders_after_breadcrumb_before_functions() {
        let ns = DocNamespace {
            name: "hot/alert".to_string(),
            namespace: "::hot::alert".to_string(),
            doc: Some("Alerting helpers for Hot.".to_string()),
            no_doc: false,
            functions: vec![DocFunction {
                name: "send-alert".to_string(),
                doc: None,
                is_core: false,
                signatures: Vec::new(),
                ctx: None,
                box_req: None,
                schedule: None,
                on_event: None,
                webhook: None,
                mcp: None,
                sends: Vec::new(),
                alias_of: None,
            }],
            types: Vec::new(),
            values: Vec::new(),
            ctx: None,
            box_req: None,
        };

        let html = generate_namespace_html(&ns, "hot.dev/hot-std");
        let breadcrumb_pos = html
            .find("<span class=\"ns-current\">::hot::alert</span>")
            .expect("breadcrumb namespace should render");
        let doc_pos = html
            .find("Alerting helpers for Hot.")
            .expect("namespace doc should render");
        let functions_pos = html
            .find("<h2>Functions</h2>")
            .expect("functions should render");

        assert!(breadcrumb_pos < doc_pos);
        assert!(doc_pos < functions_pos);
        assert!(!html.contains("<h1><code>::hot::alert</code>"));
        assert!(!html.contains("namespace-decl-badge\">namespace</span>"));
    }

    #[test]
    fn namespace_values_render_before_functions() {
        let ns = DocNamespace {
            name: "anthropic".to_string(),
            namespace: "::anthropic".to_string(),
            doc: None,
            no_doc: false,
            values: vec![DocValue {
                name: "BASE_URL".to_string(),
                doc: Some("Anthropic API base URL.".to_string()),
                value: "\"https://api.anthropic.com\"".to_string(),
                type_annotation: None,
                alias_of: None,
            }],
            functions: vec![DocFunction {
                name: "request".to_string(),
                doc: None,
                is_core: false,
                signatures: Vec::new(),
                ctx: None,
                box_req: None,
                schedule: None,
                on_event: None,
                webhook: None,
                mcp: None,
                sends: Vec::new(),
                alias_of: None,
            }],
            types: Vec::new(),
            ctx: None,
            box_req: None,
        };

        let html = generate_namespace_html(&ns, "hot.dev/anthropic");
        let values_pos = html.find("<h2>Values</h2>").expect("values should render");
        let base_url_pos = html.find("BASE_URL").expect("value should render");
        let functions_pos = html
            .find("<h2>Functions</h2>")
            .expect("functions should render");

        assert!(values_pos < base_url_pos);
        assert!(base_url_pos < functions_pos);
        assert!(html.contains("BASE_URL &quot;https://api.anthropic.com&quot;"));
        assert!(html.contains("Anthropic API base URL."));
    }

    #[test]
    fn namespace_toc_includes_all_rendered_sections_in_order() {
        let ns = DocNamespace {
            name: "demo".to_string(),
            namespace: "::demo".to_string(),
            doc: None,
            no_doc: false,
            values: vec![DocValue {
                name: "DEFAULT_TIMEOUT".to_string(),
                doc: None,
                value: "30".to_string(),
                type_annotation: Some("Int".to_string()),
                alias_of: None,
            }],
            functions: vec![DocFunction {
                name: "run".to_string(),
                doc: None,
                is_core: false,
                signatures: Vec::new(),
                ctx: None,
                box_req: None,
                schedule: None,
                on_event: None,
                webhook: None,
                mcp: None,
                sends: Vec::new(),
                alias_of: None,
            }],
            types: vec![DocType {
                name: "Config".to_string(),
                doc: None,
                is_core: false,
                fields: Vec::new(),
                constructors: Vec::new(),
                type_alias: None,
                alias_of: None,
            }],
            ctx: Some(CtxRequirements {
                req: vec!["api.key".to_string()],
                opt: Vec::new(),
            }),
            box_req: Some(BoxRequirementDoc {
                min_size: Some("small".to_string()),
                network: false,
            }),
        };

        let toc = build_namespace_toc(&ns);
        let sections: Vec<_> = toc.iter().map(|section| section.title.as_str()).collect();

        assert_eq!(
            sections,
            vec![
                "Context Vars",
                "Container Requirements",
                "Values",
                "Functions",
                "Types"
            ]
        );
        assert_eq!(toc[2].items[0].name, "DEFAULT_TIMEOUT");
        assert_eq!(toc[3].items[0].name, "run");
        assert_eq!(toc[4].items[0].name, "Config");
    }

    #[test]
    fn alias_badge_uses_alias_label_and_links_to_target() {
        let docs = PkgDocs {
            meta: PkgMeta {
                name: "hot.dev/demo".to_string(),
                ..PkgMeta::default()
            },
            readme: None,
            license: None,
            namespaces: vec![
                DocNamespace {
                    name: "lib".to_string(),
                    namespace: "::lib".to_string(),
                    doc: None,
                    no_doc: false,
                    functions: vec![DocFunction {
                        name: "target".to_string(),
                        doc: Some("Target docs.".to_string()),
                        is_core: false,
                        signatures: Vec::new(),
                        ctx: None,
                        box_req: None,
                        schedule: None,
                        on_event: None,
                        webhook: None,
                        mcp: None,
                        sends: Vec::new(),
                        alias_of: None,
                    }],
                    types: Vec::new(),
                    values: Vec::new(),
                    ctx: None,
                    box_req: None,
                },
                DocNamespace {
                    name: "facade".to_string(),
                    namespace: "::facade".to_string(),
                    doc: None,
                    no_doc: false,
                    functions: vec![DocFunction {
                        name: "target".to_string(),
                        doc: Some("Target docs.".to_string()),
                        is_core: false,
                        signatures: Vec::new(),
                        ctx: None,
                        box_req: None,
                        schedule: None,
                        on_event: None,
                        webhook: None,
                        mcp: None,
                        sends: Vec::new(),
                        alias_of: Some("::lib/target".to_string()),
                    }],
                    types: Vec::new(),
                    values: Vec::new(),
                    ctx: None,
                    box_req: None,
                },
            ],
            type_index: Default::default(),
        };
        let alias_target_index = build_alias_target_index(&docs, "hot.dev/demo", "/pkg");
        let type_index = AHashMap::new();
        let html = generate_namespace_html_with_registry_and_aliases(
            &docs.namespaces[1],
            "hot.dev/demo",
            &type_index,
            None,
            "/pkg",
            &alias_target_index,
        );

        assert!(html.contains("href=\"/pkg/hot.dev/demo/lib#target\""));
        assert!(html.contains("title=\"Alias of ::lib/target\""));
        assert!(html.contains("alias-badge\">alias</span>"));
        assert!(html.contains("<code>::lib/target</code>"));
        assert!(!html.contains("re-export"));
    }

    #[test]
    fn package_nav_nests_namespaces_as_tree() {
        let docs = PkgDocs {
            meta: PkgMeta {
                name: "hot.dev/anthropic".to_string(),
                ..PkgMeta::default()
            },
            readme: None,
            license: None,
            namespaces: vec![
                DocNamespace {
                    name: "anthropic".to_string(),
                    namespace: "::anthropic".to_string(),
                    doc: None,
                    no_doc: false,
                    functions: Vec::new(),
                    types: Vec::new(),
                    values: Vec::new(),
                    ctx: None,
                    box_req: None,
                },
                DocNamespace {
                    name: "anthropic/api".to_string(),
                    namespace: "::anthropic::api".to_string(),
                    doc: None,
                    no_doc: false,
                    functions: Vec::new(),
                    types: Vec::new(),
                    values: Vec::new(),
                    ctx: None,
                    box_req: None,
                },
                DocNamespace {
                    name: "anthropic/batches".to_string(),
                    namespace: "::anthropic::batches".to_string(),
                    doc: None,
                    no_doc: false,
                    functions: Vec::new(),
                    types: Vec::new(),
                    values: Vec::new(),
                    ctx: None,
                    box_req: None,
                },
            ],
            type_index: Default::default(),
        };

        let nav = generate_pkg_nav_with_prefix("hot.dev/anthropic", &docs, "hot.dev/anthropic");
        assert_eq!(nav.len(), 1);
        assert_eq!(nav[0].children.len(), 1);

        let anthropic = &nav[0].children[0];
        assert_eq!(anthropic.title, "::anthropic");
        assert_eq!(
            anthropic.path.as_deref(),
            Some("hot.dev/anthropic/anthropic")
        );
        assert_eq!(anthropic.children.len(), 2);
        assert_eq!(anthropic.children[0].title, "::api");
        assert_eq!(anthropic.children[1].title, "::batches");
    }
}
