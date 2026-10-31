//! Crawl the App from its main pages as a signed-in user and check every same-origin link.
//!
//! Each page must answer without an error, contain exactly one `<main>`, and close no more
//! `<div>`s than it opens before `</main>` (a surplus ends `<main>` early and breaks the skip
//! link). Links the page renders must lead somewhere: a 404 from our own navigation is a bug.
use hot::db::{TestData, insert_test_data};
use hot_app::test_support::TestClient;
use std::collections::{BTreeSet, VecDeque};

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
    pages
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
        let path = href.split(['?', '#']).next().unwrap_or_default().to_string();
        if !path.is_empty() && !SKIP_PREFIXES.iter().any(|prefix| path.starts_with(prefix)) {
            links.push(path);
        }
    }
    links
}

/// Problems with the page's `<main>`: missing or repeated, or closed early by stray `</div>`s.
fn main_problems(body: &str) -> Option<String> {
    let mains = body.matches("<main").count();
    if mains != 1 {
        return Some(format!("{mains} <main> elements"));
    }
    let start = body.find("<main").unwrap();
    let end = body[start..].find("</main>").map(|end| start + end)?;
    // Scripts and inline templates may build markup in strings; only count real tags.
    let mut html = String::new();
    let mut rest = &body[start..end];
    while let Some(index) = rest.find("<script") {
        html.push_str(&rest[..index]);
        rest = rest[index..]
            .find("</script>")
            .map_or("", |close| &rest[index + close + 9..]);
    }
    html.push_str(rest);
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

#[tokio::test]
async fn app_pages_render_one_main_and_their_links_resolve() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let org = hot::db::Org::get_org(client.db(), &td.org_id).await.unwrap();
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
        for link in page_links(&response.body) {
            if seen.insert(link.clone()) {
                queue.push_back((link, path.clone()));
            }
        }
    }
    assert!(visited > 30, "the crawl reached only {visited} pages");
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}
