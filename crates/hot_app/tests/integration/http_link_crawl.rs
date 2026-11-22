//! Crawl the App from its main pages as a signed-in user and check every same-origin link.
//!
//! Each page must answer without an error, contain exactly one `<main>`, and close no more
//! `<div>`s than it opens before `</main>` (a surplus ends `<main>` early and breaks the skip
//! link). Its element ids must be unique, and the sidebar and org tabs must highlight the
//! page's own section and tab and nothing else. Links the page renders must lead somewhere: a
//! 404 from our own navigation is a bug.
use ahash::AHashMap;
use hot::db::{Build, DatabaseScope, TestData, insert_test_data};
use hot::pkg::docs::{
    DocFunction, DocNamespace, DocType, FunctionParam, FunctionSignature, PkgDocs, PkgMeta,
    ProjectDocs, TypeField,
};
use hot_app::build_cache::{CachedBuildDocs, get_build_docs_cache, init_build_docs_cache};
use hot_app::test_support::TestClient;
use std::collections::{BTreeSet, VecDeque};
use uuid::Uuid;

const PAGE_LIMIT: usize = 250;

/// Prefixes that are actions, downloads, streams or assets rather than pages.
const SKIP_PREFIXES: &[&str] = &[
    "/assets/",
    "/auth/",
    "/signout",
    "/switch-org/",
    "/switch-env/",
    "/events/stream",
    "/data/",
    "/files/download",
    "/billing/",
];

fn start_pages(td: &TestData, slug: &str) -> Vec<String> {
    let mut pages: Vec<String> = [
        "/",
        "/runs",
        "/events",
        "/streams",
        "/tasks",
        "/files",
        "/stores",
        "/schedules",
        "/event-handlers",
        "/workflows",
        "/mcp",
        "/webhooks",
        "/projects",
        "/contexts",
        "/docs",
        "/keys",
        "/keys/new",
        "/envs",
        "/orgs",
        "/account",
        "/settings/alerts/destinations",
        "/settings/alerts/channels",
        "/settings/alerts/subscriptions",
        "/settings/alerts/history",
    ]
    .iter()
    .map(|path| path.to_string())
    .collect();
    for tail in ["", "/users", "/teams", "/usage"] {
        pages.push(format!("/@{slug}{tail}"));
    }
    pages.push(format!("/runs/{}", td.run_id));
    pages.push(format!("/streams/{}", td.stream_id));
    pages.push("/projects/test-project/docs".to_string());
    pages
}

/// Seed a deployed bundle build with documented project and dependency
/// namespaces so the link crawl exercises `/docs/{project}/project/*`,
/// `/docs/{project}/pkg/*`, and `/projects/{project}/docs`.
async fn seed_documented_build(client: &TestClient, td: &TestData) {
    let scoped = client.db().with_scope(DatabaseScope::Environment {
        org_id: td.org_id,
        env_id: td.env_id,
    });
    let docs_build_id = Uuid::now_v7();
    Build::insert_build(
        &scoped,
        &docs_build_id,
        &td.project_id,
        "docs-crawl-hash",
        128,
        Build::BUILD_TYPE_BUNDLE,
        &td.user_id,
    )
    .await
    .unwrap();
    Build::deploy_build(&scoped, &docs_build_id, &td.user_id)
        .await
        .unwrap();
    Build::mark_runtime_ready(&scoped, &docs_build_id)
        .await
        .unwrap();

    if get_build_docs_cache().is_none() {
        init_build_docs_cache(hot::val!({}));
    }
    let cache = get_build_docs_cache().expect("build docs cache initialized");
    let demo_fn = DocFunction {
        name: "greet".into(),
        doc: Some("Return a greeting.".into()),
        is_core: false,
        signatures: vec![FunctionSignature {
            params: vec![FunctionParam {
                name: "name".into(),
                type_annotation: Some("Str".into()),
                is_lazy: false,
                is_variadic: false,
            }],
            return_type: Some("Str".into()),
            precondition: None,
        }],
        ctx: None,
        box_req: None,
        schedule: None,
        on_event: None,
        webhook: None,
        mcp: None,
        sends: vec![],
        alias_of: None,
    };
    let demo_type = DocType {
        name: "Greeting".into(),
        doc: Some("A greeting payload.".into()),
        is_core: false,
        fields: vec![TypeField {
            name: "message".into(),
            type_annotation: Some("Str".into()),
        }],
        constructors: vec![],
        type_alias: None,
        refinement: None,
        alias_of: None,
    };
    let project_ns = DocNamespace {
        name: "demo".into(),
        namespace: "::demo".into(),
        doc: Some("Demo project namespace.".into()),
        no_doc: false,
        functions: vec![demo_fn.clone()],
        types: vec![demo_type],
        values: vec![],
        ctx: None,
        box_req: None,
    };
    let pkg_ns = DocNamespace {
        name: "demo-pkg".into(),
        namespace: "::demo-pkg".into(),
        doc: Some("Demo package namespace.".into()),
        no_doc: false,
        functions: vec![demo_fn],
        types: vec![],
        values: vec![],
        ctx: None,
        box_req: None,
    };
    let mut dependency_docs = AHashMap::new();
    dependency_docs.insert(
        "hot.dev/demo-pkg".to_string(),
        PkgDocs {
            meta: PkgMeta {
                org: "hot.dev".into(),
                name: "demo-pkg".into(),
                version: "1.0.0".into(),
                description: "Demo dependency package".into(),
                ..Default::default()
            },
            readme: Some("# Demo Pkg\n\nExample README.".into()),
            license: None,
            namespaces: vec![pkg_ns],
            type_index: AHashMap::new(),
        },
    );
    cache
        .insert_test_docs(CachedBuildDocs {
            build_id: docs_build_id,
            project_docs: ProjectDocs {
                name: "test-project".into(),
                namespaces: vec![project_ns],
                type_index: AHashMap::new(),
            },
            dependency_docs,
            cached_at: std::time::Instant::now(),
        })
        .await;
}

/// Same-origin `href`s in a page, without query strings or fragments.
fn page_links(body: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = body;
    while let Some(index) = rest.find("href=\"") {
        rest = &rest[index + 6..];
        let Some(end) = rest.find('"') else { break };
        let href = rest[..end].replace("&amp;", "&");
        rest = &rest[end..];
        if !href.starts_with('/') || href.starts_with("//") || href.contains("${") {
            continue;
        }
        let path = href
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_string();
        if !path.is_empty() && !SKIP_PREFIXES.iter().any(|prefix| path.starts_with(prefix)) {
            links.push(path);
        }
    }
    links
}

/// `html` without the elements that open with `open` and close with `close`.
fn without(html: &str, open: &str, close: &str) -> String {
    let mut kept = String::new();
    let mut rest = html;
    while let Some(index) = rest.find(open) {
        kept.push_str(&rest[..index]);
        rest = rest[index..]
            .find(close)
            .map_or("", |end| &rest[index + end + close.len()..]);
    }
    kept.push_str(rest);
    kept
}

/// Problems with the page's `<main>`: missing or repeated, or closed early by stray `</div>`s.
fn main_problems(body: &str) -> Option<String> {
    let stripped = without(&without(body, "<script", "</script>"), "<!--", "-->");
    let mains = stripped.matches("<main").count();
    if mains != 1 {
        return Some(format!("{mains} <main> elements"));
    }
    let start = stripped.find("<main").unwrap();
    let end = stripped[start..].find("</main>").map(|end| start + end)?;
    let html = &stripped[start..end];
    let mut depth = 0i64;
    let mut position = 0;
    while let Some(index) = html[position..].find("div") {
        let at = position + index;
        if html[..at].ends_with("</") {
            depth -= 1;
        } else if html[..at].ends_with('<') {
            depth += 1;
        }
        if depth < 0 {
            let context = &html[at.saturating_sub(200)..at];
            return Some(format!("a </div> closes <main> early, after: …{context}"));
        }
        position = at + 3;
    }
    (depth != 0).then(|| format!("{depth} <div>s are still open at </main>"))
}

/// Ids the page repeats. Scripts and inert `<template>`s are left out: Alpine stamps their
/// copies at runtime.
fn duplicate_ids(body: &str) -> Vec<String> {
    let html = without(
        &without(body, "<script", "</script>"),
        "<template",
        "</template>",
    );
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    let mut position = 0;
    while let Some(index) = html[position..].find("id=\"") {
        let at = position + index;
        position = at + 4;
        // A bare `id` attribute, not `data-id`, `:id` or `aria-labelledby`'s tail.
        if !html[..at].ends_with(char::is_whitespace) {
            continue;
        }
        let Some(end) = html[position..].find('"') else {
            break;
        };
        let id = &html[position..position + end];
        if !seen.insert(id) {
            duplicates.insert(id.to_string());
        }
    }
    duplicates.into_iter().collect()
}

/// The sidebar sections: each link's path and the paths it covers.
const SIDEBAR_SECTIONS: &[(&str, &str)] = &[
    ("/", "/"),
    ("/workflows", "/workflows"),
    ("/runs", "/runs"),
    ("/tasks", "/tasks"),
    ("/events", "/events"),
    ("/streams", "/streams"),
    ("/files", "/files"),
    ("/stores", "/stores"),
    ("/schedules", "/schedules"),
    ("/event-handlers", "/event-handlers"),
    ("/mcp", "/mcp"),
    ("/webhooks", "/webhooks"),
    ("/settings/alerts/destinations", "/settings/alerts"),
    ("/projects", "/projects"),
    ("/contexts", "/contexts"),
    ("/docs", "/docs"),
    ("/keys", "/keys"),
    ("/service-keys", "/service-keys"),
    ("/domains", "/domains"),
];

/// The part of `body` from `open` to the next `close`.
fn element<'a>(body: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = body.find(open)?;
    let end = body[start..]
        .find(close)
        .map_or(body.len(), |end| start + end);
    Some(&body[start..end])
}

/// Every link in `nav` with its `href`, and whether its opening tag has the class `active`.
fn nav_links<'a>(nav: &'a str, active: &str) -> Vec<(&'a str, bool)> {
    let mut links = Vec::new();
    let mut position = 0;
    while let Some(index) = nav[position..].find("<a href=\"") {
        let at = position + index + 9;
        position = at;
        let Some(href_end) = nav[at..].find('"') else {
            break;
        };
        let tag_end = nav[at..].find('>').map_or(nav.len(), |end| at + end);
        let is_active = nav[at..tag_end]
            .split(|c: char| c.is_whitespace() || c == '"')
            .any(|class| class == active);
        links.push((&nav[at..at + href_end], is_active));
    }
    links
}

/// What is wrong with a nav's highlight, given the link it should highlight, if any.
fn highlight_problem(nav: &str, expected: Option<&str>, links: &[(&str, bool)]) -> Option<String> {
    let active: Vec<&str> = links
        .iter()
        .filter(|(_, is_active)| *is_active)
        .map(|(href, _)| *href)
        .collect();
    match (expected, active.as_slice()) {
        (_, [_, _, ..]) => Some(format!("the {nav} highlights {}", active.join(", "))),
        (Some(href), []) => Some(format!("the {nav} doesn't highlight {href}")),
        (Some(href), [shown]) if *shown != href => {
            Some(format!("the {nav} highlights {shown}, not {href}"))
        }
        (None, [shown]) => Some(format!("the {nav} highlights {shown}")),
        _ => None,
    }
}

/// Whether `path` is `prefix` or a page under it.
fn is_under(path: &str, prefix: &str) -> bool {
    path == prefix || (prefix != "/" && path.starts_with(&format!("{prefix}/")))
}

/// Problems with the highlights: the sidebar must mark the page's section, and the org tabs
/// the page's tab, and neither may mark more than one.
fn nav_problems(path: &str, body: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(sidebar) = element(body, "<aside id=\"sidebar\"", "</aside>") {
        let expected = SIDEBAR_SECTIONS
            .iter()
            .filter(|(_, prefix)| is_under(path, prefix))
            .max_by_key(|(_, prefix)| prefix.len())
            .map(|(href, _)| *href);
        let links = nav_links(sidebar, "sidebar-active");
        problems.extend(highlight_problem("sidebar", expected, &links));
    }
    if let Some(tabs) = element(body, "aria-label=\"Organization navigation\"", "</nav>") {
        let links = nav_links(tabs, "border-red-500");
        let expected = links
            .iter()
            .map(|(href, _)| *href)
            .filter(|href| is_under(path, href))
            .max_by_key(|href| href.len());
        problems.extend(highlight_problem("org tabs", expected, &links));
    }
    problems
}

#[test]
fn the_checks_catch_repeated_ids_and_wrong_highlights() {
    let ids = r#"<div id="a"></div><p
        id="a"></p><span data-id="a" :id="b"></span><template><i id="c"></i><i id="c"></i></template>"#;
    assert_eq!(duplicate_ids(ids), vec!["a"]);

    let page = r#"<aside id="sidebar"><nav>
        <a href="/runs" class="flex sidebar-active border-l-2">Runs</a>
        <a href="/events" class="flex text-gray-800">Events</a>
        <a href="/settings/alerts/destinations" class="flex">Alerts</a>
    </nav></aside>"#;
    assert!(nav_problems("/runs/42", page).is_empty());
    assert_eq!(
        nav_problems("/events", page),
        vec!["the sidebar highlights /runs, not /events"]
    );
    assert_eq!(
        nav_problems("/settings/alerts/channels", page),
        vec!["the sidebar highlights /runs, not /settings/alerts/destinations"]
    );
    assert_eq!(
        nav_problems("/account", page),
        vec!["the sidebar highlights /runs"]
    );
}

#[tokio::test]
async fn app_pages_render_one_main_and_their_links_resolve() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    seed_documented_build(&client, &td).await;
    let org = hot::db::Org::get_org(client.db(), &td.org_id)
        .await
        .unwrap();
    client.login_as(&td.user_id);

    let mut queue: VecDeque<(String, String)> = start_pages(&td, &org.slug)
        .into_iter()
        .map(|page| (page, "start".to_string()))
        .collect();
    let mut seen: BTreeSet<String> = queue.iter().map(|(page, _)| page.clone()).collect();
    let mut problems = Vec::new();
    let mut visited = 0;
    while let Some((path, from)) = queue.pop_front() {
        if visited == PAGE_LIMIT {
            break;
        }
        visited += 1;
        let response = client.get(&path).await;
        if response.status.is_redirection() {
            continue;
        }
        if !response.status.is_success() {
            problems.push(format!("{path} (linked from {from}): {}", response.status));
            continue;
        }
        let is_html = response
            .headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html"));
        if !is_html || !response.body.contains("<html") {
            continue;
        }
        if let Some(problem) = main_problems(&response.body) {
            problems.push(format!("{path}: {problem}"));
        }
        let duplicates = duplicate_ids(&response.body);
        if !duplicates.is_empty() {
            problems.push(format!("{path}: repeated ids {}", duplicates.join(", ")));
        }
        for problem in nav_problems(&path, &response.body) {
            problems.push(format!("{path}: {problem}"));
        }
        for link in page_links(&response.body) {
            if seen.insert(link.clone()) {
                queue.push_back((link, path.clone()));
            }
        }
    }
    assert!(visited > 30, "the crawl reached only {visited} pages");
    assert!(
        seen.iter()
            .any(|p| p.starts_with("/docs/test-project/project/")),
        "the crawl did not reach any project namespace docs page"
    );
    assert!(
        seen.iter()
            .any(|p| p.starts_with("/docs/test-project/pkg/")),
        "the crawl did not reach any package docs page"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}
