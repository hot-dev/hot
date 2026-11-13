//! Keeps the configuration docs, the `all` template and the defaults in step.
//!
//! `resources/init/hot.hot.all.template` is the list of settings: `hot conf
//! generate all` prints it, as live lines or as commented examples.
//!
//! - Every `hot.<key>` the docs name is a setting in the template.
//! - Every setting in the template is named in the docs: by its key, with
//!   or without the `hot.`, or by the environment variable the template
//!   reads it from.
//! - A default in a docs table is the default Hot starts with.
//!
//! The exceptions are the three lists below. Each entry says why.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// `hot.<something>` in the docs that is not a `hot.hot` setting.
/// (page, name, what it is). A page of `*` is every page.
const NOT_SETTINGS: &[(&str, &str, &str)] = &[
    ("*", "hot", "the file hot.hot"),
    ("*", "dev", "the site hot.dev"),
    ("*", "request", "the `hot.request` context value"),
    ("editor/index.md", "lsp.enabled", "a VS Code setting"),
];

/// Settings the docs name that the template leaves out. (key, why).
const NOT_IN_TEMPLATE: &[(&str, &str)] = &[
    (
        "profile.set",
        "the old spelling of hot.set.profile, kept for existing files",
    ),
    (
        "test.integration.services",
        "read from a package's integration.hot, not from hot.hot",
    ),
];

/// Template settings the docs leave out. (key or `prefix.*`, why).
const NOT_IN_DOCS: &[(&str, &str)] = &[
    (
        "stripe.*",
        "billing belongs to the hosted service; the open source build ignores it",
    ),
    ("billing.enabled", "as stripe.*"),
    (
        "product.web-url",
        "a link of the hosted service, shown in the app and in email",
    ),
    ("product.pricing-url", "as product.web-url"),
    ("product.support-email", "as product.web-url"),
    (
        "domain.aws.*",
        "read by a domain provider, and Hot ships none",
    ),
    // The template sets these four, and no code reads them: caching was
    // removed, and S3 takes its region from `*.aws.s3.region`. Remove each
    // here when it leaves the template.
    ("cache.enabled", "no code reads it"),
    ("cache.format", "no code reads it"),
    ("build.aws.region", "no code reads it"),
    ("file.aws.region", "no code reads it"),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `project.my-app.deps` -> `project.<name>.deps`, so a page's example
/// project and the template's placeholder compare equal.
fn normalize(key: &str) -> String {
    let mut parts: Vec<&str> = key.split('.').collect();
    if parts.len() > 2 && matches!(parts[0], "project" | "profile" | "remote") {
        parts[1] = "<name>";
    }
    parts.join(".")
}

fn is_key_text(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            part == "*"
                || part == "<name>"
                || part == "{{PROJECT_NAME}}"
                || (part.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        })
}

/// The key a `hot.hot` line sets: `hot.a.b value` or `// hot.a.b value`.
fn setting_line_key(line: &str) -> Option<String> {
    let line = line.trim().trim_start_matches('/').trim_start();
    let rest = line.strip_prefix("hot.")?;
    let (key, value) = rest.split_once(char::is_whitespace)?;
    (is_key_text(key) && !value.trim().is_empty()).then(|| normalize(key))
}

/// Template key -> the environment variable its line reads, if any.
fn template_settings() -> BTreeMap<String, Option<String>> {
    let path = repo_root().join("resources/init/hot.hot.all.template");
    let mut settings = BTreeMap::new();
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let Some(key) = setting_line_key(line) else {
            continue;
        };
        let variable = line
            .split_once("::env/get(\"")
            .and_then(|(_, rest)| rest.split('"').next())
            .map(str::to_string);
        let entry = settings.entry(key).or_insert(None);
        if entry.is_none() {
            *entry = variable;
        }
    }
    settings
}

struct Mention {
    key: String,
    page: String,
    line: usize,
}

/// What the docs say: the settings they name, the environment variables
/// they name, and the defaults their tables give.
struct Docs {
    mentions: Vec<Mention>,
    /// Code spans that read as a key without `hot.`, such as
    /// `worker.run-timeout`. Prose often names a setting this way.
    bare: BTreeSet<String>,
    variables: BTreeSet<String>,
    /// (key, default as written, page, line) from `| `hot.key` | ... | `default` |` rows.
    defaults: Vec<(String, String, String, usize)>,
}

fn code_spans(line: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        let body = &rest[start + 1..];
        let Some(end) = body.find('`') else { break };
        spans.push(&body[..end]);
        rest = &body[end + 1..];
    }
    spans
}

fn read_docs() -> Docs {
    let root = repo_root().join("resources/docs");
    let mut mentions = Vec::new();
    let mut bare = BTreeSet::new();
    let mut variables = BTreeSet::new();
    let mut defaults = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let page = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            let mut fence: Option<String> = None;
            let mut manifest = false;
            for (index, line) in text.lines().enumerate() {
                let number = index + 1;
                let trimmed = line.trim();
                if trimmed.starts_with("```") {
                    fence = match fence {
                        Some(_) => None,
                        None => Some(trimmed.trim_start_matches('`').trim().to_string()),
                    };
                    manifest = false;
                    continue;
                }
                let mut mention = |key: String| {
                    mentions.push(Mention {
                        key,
                        page: page.clone(),
                        line: number,
                    })
                };
                match fence.as_deref() {
                    // A `hot` block that sets keys is a piece of hot.hot,
                    // unless it is a package manifest.
                    Some("hot") => {
                        manifest |= trimmed.starts_with("::hot::pkg ");
                        if !manifest
                            && !trimmed.starts_with("//")
                            && let Some(key) = setting_line_key(trimmed)
                            && !key.starts_with("pkg.")
                        {
                            mention(key);
                        }
                    }
                    Some(_) => {}
                    None => {
                        let spans = code_spans(line);
                        for span in &spans {
                            // `hot.check.raw true` names the setting too.
                            if let Some(key) = span
                                .strip_prefix("hot.")
                                .and_then(|rest| rest.split_whitespace().next())
                                && is_key_text(key)
                                && !key.starts_with("pkg.")
                                && !key.ends_with(".hot")
                                && !key.starts_with("sqlite.")
                            {
                                mention(normalize(key));
                            }
                            if span.contains('.') && is_key_text(span) {
                                bare.insert(normalize(span));
                            }
                            if span.starts_with("HOT_")
                                && span.chars().all(|c| {
                                    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
                                })
                            {
                                variables.insert(span.to_string());
                            }
                        }
                        let cells: Vec<&str> = trimmed.split('|').map(str::trim).collect();
                        // `| `email.provider` | `HOT_EMAIL_PROVIDER` | ...`:
                        // a setting's row may leave the `hot.` off.
                        fn cell_code<'a>(cell: &&'a str) -> Option<&'a str> {
                            cell.strip_prefix('`')?.strip_suffix('`')
                        }
                        if let Some(key) = cells.get(1).and_then(cell_code)
                            && let Some(variable) = cells.get(2).and_then(cell_code)
                            && variable.starts_with("HOT_")
                            && !key.starts_with("hot.")
                            && is_key_text(key)
                        {
                            mention(normalize(key));
                        }
                        // A table row that starts with a key and ends with
                        // a default written as one code span.
                        if cells.len() >= 5
                            && let Some(key) = cells[1]
                                .strip_prefix("`hot.")
                                .and_then(|cell| cell.strip_suffix('`'))
                            && is_key_text(key)
                            && let Some(default) = cells[cells.len() - 2]
                                .strip_prefix('`')
                                .and_then(|cell| cell.strip_suffix('`'))
                            && !default.contains('`')
                        {
                            defaults.push((
                                key.to_string(),
                                default.to_string(),
                                page.clone(),
                                number,
                            ));
                        }
                    }
                }
            }
        }
    }
    Docs {
        mentions,
        bare,
        variables,
        defaults,
    }
}

fn excepted(list: &[(&str, &str)], key: &str) -> bool {
    list.iter()
        .any(|(entry, _)| match entry.strip_suffix(".*") {
            Some(prefix) => key.starts_with(&format!("{prefix}.")),
            None => *entry == key,
        })
}

#[test]
fn every_documented_setting_is_in_the_template() {
    let template = template_settings();
    let docs = read_docs();
    let mut problems = BTreeSet::new();
    for Mention { key, page, line } in &docs.mentions {
        if NOT_SETTINGS.iter().any(|(p, name, _)| {
            (*p == "*" || p == page) && (name == key || key.starts_with(&format!("{name}.")))
        }) || excepted(NOT_IN_TEMPLATE, key)
        {
            continue;
        }
        // `hot.scheduler.*` and `hot.db.sqlite` name a group of settings.
        let group = key.trim_end_matches(".*");
        let known = template.contains_key(key)
            || template
                .keys()
                .any(|setting| setting.starts_with(&format!("{group}.")));
        if !known {
            problems.insert(format!(
                "{page}:{line}: `hot.{key}` is not in resources/init/hot.hot.all.template"
            ));
        }
    }
    assert!(
        docs.mentions.len() >= 300,
        "found only {} settings in the docs",
        docs.mentions.len()
    );
    assert!(
        problems.is_empty(),
        "\n{}\nAdd the setting to the template (a commented line will do), or list it in \
         NOT_SETTINGS or NOT_IN_TEMPLATE in crates/hot_cli/src/conf_docs.rs.",
        problems.into_iter().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn every_template_setting_is_documented() {
    let template = template_settings();
    let docs = read_docs();
    let named: BTreeSet<&str> = docs.mentions.iter().map(|m| m.key.as_str()).collect();
    let mut problems = Vec::new();
    for (key, variable) in &template {
        let documented = named.contains(key.as_str())
            || docs.bare.contains(key)
            || variable
                .as_ref()
                .is_some_and(|variable| docs.variables.contains(variable));
        if !documented && !excepted(NOT_IN_DOCS, key) {
            problems.push(match variable {
                Some(variable) => format!("`hot.{key}` (or `{variable}`)"),
                None => format!("`hot.{key}`"),
            });
        }
    }
    assert!(
        template.len() >= 150,
        "found only {} template settings",
        template.len()
    );
    assert!(
        problems.is_empty(),
        "resources/docs names none of these template settings:\n{}\nDocument each under \
         resources/docs/configuration, or list it in NOT_IN_DOCS in crates/hot_cli/src/conf_docs.rs.",
        problems.join("\n")
    );
}

/// Exceptions must stay true, or they hide the next drift.
#[test]
fn exception_lists_hold_only_what_they_need() {
    let template = template_settings();
    let docs = read_docs();
    let named: BTreeSet<&str> = docs.mentions.iter().map(|m| m.key.as_str()).collect();
    for (key, _) in NOT_IN_TEMPLATE {
        assert!(!template.contains_key(*key), "{key} is in the template now");
        assert!(named.contains(key), "the docs no longer name {key}");
    }
    for (entry, _) in NOT_IN_DOCS {
        let covered: Vec<&String> = template
            .keys()
            .filter(|key| excepted(&[(*entry, "")], key))
            .collect();
        assert!(
            !covered.is_empty(),
            "{entry} matches nothing in the template"
        );
        for key in covered {
            assert!(
                !named.contains(key.as_str()) && !docs.bare.contains(key),
                "the docs name {key} now"
            );
        }
    }
    for (page, name, _) in NOT_SETTINGS {
        assert!(
            docs.mentions
                .iter()
                .any(|m| (*page == "*" || m.page == *page) && m.key == *name),
            "{page} no longer names hot.{name}"
        );
    }
}

/// How a default reads in a docs table.
fn written(value: &Val) -> Option<String> {
    match value {
        Val::Str(text) => Some(text.to_string()),
        Val::Int(number) => Some(number.to_string()),
        Val::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

#[test]
fn documented_defaults_are_the_defaults() {
    let defaults = create_default_conf();
    let docs = read_docs();
    let mut problems = Vec::new();
    let mut checked = 0;
    for (key, documented, page, line) in &docs.defaults {
        // A setting with no default, or one resolved later (an empty
        // string here), has nothing to compare.
        let Some(actual) = defaults.get(key).as_ref().and_then(written) else {
            continue;
        };
        if actual.is_empty() {
            continue;
        }
        checked += 1;
        // A relative directory is resolved against where Hot runs. On
        // Windows that gives `D:\dir\.hot/log`, so compare with `/` only.
        let resolved = std::path::Path::new(&actual).is_absolute()
            && actual
                .replace('\\', "/")
                .ends_with(&format!("/{}", documented.trim_start_matches("./")));
        if &actual != documented && !resolved {
            problems.push(format!(
                "{page}:{line}: `hot.{key}` defaults to `{actual}`, not `{documented}`"
            ));
        }
    }
    assert!(checked >= 40, "compared only {checked} documented defaults");
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn setting_lines_are_read_live_or_commented() {
    assert_eq!(
        setting_line_key("hot.db.uri ::env/get(\"HOT_DB_URI\", \"x\")").as_deref(),
        Some("db.uri")
    );
    assert_eq!(
        setting_line_key("//   hot.project.{{PROJECT_NAME}}.skills.codegen.enabled   true")
            .as_deref(),
        Some("project.<name>.skills.codegen.enabled")
    );
    assert_eq!(
        setting_line_key("// hot.blob.mode \"service\"").as_deref(),
        Some("blob.mode")
    );
    // Prose that mentions a file or a command is not a setting.
    assert_eq!(
        setting_line_key("// hot.hot, set the keys themselves."),
        None
    );
    assert_eq!(
        setting_line_key("// hot dev exclusively locks the queue"),
        None
    );
    assert_eq!(
        setting_line_key("hot.pkg.my-package {"),
        Some("pkg.my-package".to_string())
    );
}
