//! Regression corpus: run the formatter over every tracked `.hot` file in
//! the repository (packages, project sources, tests, docs examples, skill
//! examples, benchmarks) and the `hot init` starter templates, plus mutated
//! copies of them (two-space indentation, one-line `type` bodies, one-line
//! `meta` blocks).
//!
//! For every case the formatter must succeed (its guard proves the result
//! parses to the same program, with byte-identical string literals) and be
//! idempotent: formatting the result again changes nothing.

use super::*;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// Tracked `.hot` files, or every `.hot` file the discovery walk finds when
/// git is unavailable.
fn corpus_files(root: &Path) -> Vec<PathBuf> {
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "-z", "--", "*.hot"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            out.stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| root.join(String::from_utf8_lossy(p).as_ref()))
                .filter(|p| p.is_file())
                .collect::<Vec<_>>()
        });
    match tracked {
        Some(files) if !files.is_empty() => files,
        _ => crate::discovery::discover(
            &[root],
            &crate::discovery::DiscoveryOpts::for_extension("hot"),
        )
        .into_iter()
        .map(|d| d.abs_path)
        .collect(),
    }
}

/// The `hot init` templates that are Hot source once rendered.
fn starter_templates(root: &Path) -> Vec<(String, String)> {
    let dir = root.join("resources/init");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("resources/init") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !(name.starts_with("hi.hot") || name.starts_with("hot.hot"))
            || !name.ends_with(".template")
        {
            continue;
        }
        let rendered = render_template(&std::fs::read_to_string(&path).expect("template"));
        if parses(&rendered) {
            out.push((format!("resources/init/{name}"), rendered));
        }
    }
    out
}

pub(super) fn render_template(template: &str) -> String {
    template
        .replace("{{PROJECT_NAME}}", "corpus")
        .replace("{{ENV_NAME}}", "development")
        .replace("{{ORG_SLUG}}", "corpus-org")
        .replace("{{USER_EMAIL}}", "dev@example.com")
}

/// Mutation: halve the leading-space indentation of every line.
fn halve_indentation(source: &str) -> String {
    source
        .split('\n')
        .map(|line| {
            let spaces = line.len() - line.trim_start_matches(' ').len();
            format!("{}{}", " ".repeat(spaces / 2), &line[spaces..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Mutation: put multi-line blocks that open with a line matching `opens`
/// and close with `}` at column 0 on one line, when their body has no
/// comments or string delimiters that could span lines.
fn collapse_blocks(source: &str, opens: impl Fn(&str) -> bool) -> Option<String> {
    let lines: Vec<&str> = source.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if opens(line) {
            let mut j = i + 1;
            let mut body = Vec::new();
            let mut ok = true;
            while j < lines.len() && lines[j] != "}" {
                let t = lines[j];
                if ["//", "/*", "\"\"\"", "`"].iter().any(|x| t.contains(x)) || !t.starts_with(' ')
                {
                    ok = false;
                    break;
                }
                body.push(t.trim());
                j += 1;
            }
            if ok && j < lines.len() && !body.is_empty() {
                out.push(format!("{} {} }}", line, body.join(" ")));
                changed = true;
                i = j + 1;
                continue;
            }
        }
        out.push(line.to_string());
        i += 1;
    }
    changed.then(|| out.join("\n"))
}

fn collapse_type_bodies(source: &str) -> Option<String> {
    collapse_blocks(source, |line| {
        (!line.starts_with(' ') && line.ends_with(" type {")) || line == "type {"
    })
}

fn collapse_meta_blocks(source: &str) -> Option<String> {
    collapse_blocks(source, |line| line == "meta {")
}

/// Format `source` twice; record a failure unless both succeed and agree.
fn check_case(name: &str, source: &str, failures: &mut Vec<String>) {
    let once = match format_str(source, 4) {
        Ok(once) => once,
        Err(e) => {
            failures.push(format!("{name}: {e}"));
            return;
        }
    };
    match format_str(&once, 4) {
        Ok(twice) if twice == once => {}
        Ok(twice) => {
            let line = once
                .lines()
                .zip(twice.lines())
                .position(|(a, b)| a != b)
                .unwrap_or(0)
                + 1;
            failures.push(format!(
                "{name}: not idempotent (second pass differs at line {line})"
            ));
        }
        Err(e) => failures.push(format!("{name}: second pass failed: {e}")),
    }
}

fn parses(source: &str) -> bool {
    crate::lang::parser::parse_hot(source).is_ok()
}

#[test]
fn repo_corpus_formats_safely_and_idempotently() {
    let root = repo_root();
    if !root.join("hot/pkg").is_dir() || !root.join("resources/init").is_dir() {
        eprintln!(
            "skipping: {} is not a Hot repository checkout",
            root.display()
        );
        return;
    }
    let files = corpus_files(&root);
    assert!(
        files.len() > 100,
        "expected the repository's .hot files, found {}",
        files.len()
    );

    let mut sources: Vec<(String, String)> = files
        .iter()
        .map(|path| {
            let name = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .display()
                .to_string();
            let source =
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {name}: {e}"));
            (name, source)
        })
        .collect();
    sources.extend(starter_templates(&root));

    let mut failures = Vec::new();
    let mut cases = 0usize;
    for (name, source) in &sources {
        cases += 1;
        check_case(name, source, &mut failures);

        let mutations = [
            ("two-space indentation", Some(halve_indentation(source))),
            ("one-line type bodies", collapse_type_bodies(source)),
            ("one-line meta blocks", collapse_meta_blocks(source)),
        ];
        for (label, mutated) in mutations {
            if let Some(mutated) = mutated
                && mutated != *source
                && parses(&mutated)
            {
                cases += 1;
                check_case(&format!("{name} [{label}]"), &mutated, &mut failures);
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {cases} corpus cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
