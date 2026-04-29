//! Build-time secret-shape scanner.
//!
//! Runs over the contents of a built bundle (source files + resources) and
//! flags entries that *look like* common credentials so they don't get
//! shipped to the cloud by accident. This is intentionally a conservative
//! pattern set — we only fire on shapes that have very low false-positive
//! rates in source/resource bundles (provider-prefixed API keys, AWS access
//! key IDs, JWTs, PEM private keys, etc.).
//!
//! ## Allowlist
//!
//! Two escape hatches, both surfaced in the build output so they're never
//! invisible:
//!
//! 1. **Per-file allowlist** in `hot.hot`:
//!    ```hot
//!    hot.build.allow-secret-shape ["resources/fixtures/*.json"]
//!    ```
//!    Patterns are interpreted as gitignore-style globs against the
//!    in-bundle relative path of each file (so `resources/...` matches a
//!    bundled resource and bare paths like `hot/src/foo.hot` match source
//!    files). Negative patterns (`!`) are supported.
//!
//! 2. **CLI override** `--allow-secret-shape` on `hot build`/`hot deploy`,
//!    which behaves as if every file were allowlisted (i.e. completely
//!    skips the scan). Use sparingly; intended for one-off bypass.
//!
//! ## Where this runs
//!
//! [`scan_bundle`] is invoked from `crate::build::create_build` after files
//! and resources have been collected but before the zip is written, so we
//! can fail fast without producing throwaway artifacts.

use crate::bundle::{BundleFile, BundleResource};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::Path;

/// One detected secret-shape match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMatch {
    /// The bundle-relative path of the file the match was found in.
    pub path: String,
    /// Short human-readable label for the secret kind ("aws-access-key",
    /// "stripe-live-key", etc.).
    pub kind: &'static str,
    /// 1-based line number of the match.
    pub line: usize,
    /// Surrounding text snippet, with the matched substring partially
    /// redacted so we don't echo the secret itself back into logs.
    pub snippet: String,
}

/// Scan options derived from project config + CLI flags.
#[derive(Debug, Default, Clone)]
pub struct SecretScanOpts {
    /// Gitignore-style patterns matched against bundle-relative paths. A
    /// matching file is *skipped* by the scanner.
    pub allow_patterns: Vec<String>,
    /// If true, the scanner is a no-op. Wired to the `--allow-secret-shape`
    /// CLI flag.
    pub allow_all: bool,
}

impl SecretScanOpts {
    /// Build an options bundle from the provided allowlist patterns and
    /// CLI override.
    pub fn new(allow_patterns: Vec<String>, allow_all: bool) -> Self {
        Self {
            allow_patterns,
            allow_all,
        }
    }
}

/// Scan a built bundle's contents for secret shapes.
///
/// Returns the list of matches in deterministic order (path, then line).
/// The caller is responsible for turning a non-empty result into a build
/// error and rendering the message to the user.
pub fn scan_bundle(
    files: &[BundleFile],
    resources: &[BundleResource],
    opts: &SecretScanOpts,
) -> Result<Vec<SecretMatch>, String> {
    if opts.allow_all {
        return Ok(Vec::new());
    }
    let allowlist = build_allowlist(&opts.allow_patterns)?;
    let mut hits: Vec<SecretMatch> = Vec::new();

    for f in files {
        if is_allowed(&allowlist, &f.relative_path) {
            continue;
        }
        scan_bytes(&f.relative_path, &f.content, &mut hits);
    }
    for r in resources {
        // Resources land in the bundle under `resources/<rel_path>`; match
        // the same prefix users see in the manifest so allowlist patterns
        // are intuitive.
        let bundle_path = format!("resources/{}", r.rel_path);
        if is_allowed(&allowlist, &bundle_path) {
            continue;
        }
        scan_bytes(&bundle_path, &r.content, &mut hits);
    }

    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(hits)
}

/// Render scan hits into a single user-facing build-error message.
pub fn format_findings(hits: &[SecretMatch]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "found {} potential secret(s) in bundled content:\n",
        hits.len()
    ));
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "  {:>2}. {} (line {}) — {}\n      {}\n",
            i + 1,
            h.path,
            h.line,
            h.kind,
            h.snippet,
        ));
    }
    out.push_str(
        "\nIf these are intentional (test fixtures, example configs, public \
         keys, …), allowlist them in hot.hot:\n  \
         hot.build.allow-secret-shape [\"path/to/file\"]\n\
         …or pass `--allow-secret-shape` to bypass the scan for this build.\n\
         Otherwise: rotate the credential and remove it from the bundle. \
         Production secrets belong in `::hot::ctx`, not in source.",
    );
    out
}

fn build_allowlist(patterns: &[String]) -> Result<Gitignore, String> {
    // The `ignore` crate's gitignore parser needs an anchor directory; the
    // bundle root is conceptually the project root, so use `.` as a stable
    // anchor — patterns are matched against bundle-relative paths.
    let mut b = GitignoreBuilder::new(Path::new("."));
    for p in patterns {
        if let Err(e) = b.add_line(None, p) {
            return Err(format!("invalid allow-secret-shape pattern {:?}: {}", p, e));
        }
    }
    b.build()
        .map_err(|e| format!("failed to build allow-secret-shape matcher: {}", e))
}

fn is_allowed(allowlist: &Gitignore, rel_path: &str) -> bool {
    // `matched_path_or_any_parents` treats paths that ignore-as-dir match
    // any parent dir as ignored, which is what users expect from glob
    // allowlists like `resources/fixtures/`.
    matches!(
        allowlist.matched_path_or_any_parents(Path::new(rel_path), false),
        ignore::Match::Ignore(_)
    )
}

fn scan_bytes(path: &str, bytes: &[u8], out: &mut Vec<SecretMatch>) {
    // Skip clearly-binary content: regex over arbitrary bytes is wasted
    // work and the patterns we care about are all ASCII anyway.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    for (line_idx, line) in text.lines().enumerate() {
        for det in &*DETECTORS {
            if let Some(m) = det.regex.find(line) {
                out.push(SecretMatch {
                    path: path.to_string(),
                    kind: det.kind,
                    line: line_idx + 1,
                    snippet: redact(line, m.start(), m.end()),
                });
            }
        }
    }
}

/// Replace the middle of a matched range with `***` so the secret itself
/// doesn't end up in build logs / CI output. Show the first and last 4
/// chars only, which is enough to identify the issue without leaking it.
fn redact(line: &str, start: usize, end: usize) -> String {
    let prefix = &line[..start];
    let matched = &line[start..end];
    let suffix = &line[end..];
    let redacted = if matched.chars().count() > 12 {
        let head: String = matched.chars().take(4).collect();
        let tail: String = matched.chars().rev().take(4).collect::<String>();
        let tail: String = tail.chars().rev().collect();
        format!("{}***{}", head, tail)
    } else {
        "***".to_string()
    };
    let combined = format!("{}{}{}", prefix, redacted, suffix);
    // Trim very long lines to keep the message readable.
    if combined.len() > 160 {
        format!("{}…", &combined[..160])
    } else {
        combined
    }
}

struct Detector {
    kind: &'static str,
    regex: Regex,
}

/// Conservative provider-prefixed secret patterns. Each one is anchored on
/// a vendor-specific prefix so we get near-zero false positives; we don't
/// try to detect "looks like base64" or arbitrary high-entropy strings
/// (those produce too many false hits in real projects).
static DETECTORS: Lazy<Vec<Detector>> = Lazy::new(|| {
    let mk = |kind, pat: &str| Detector {
        kind,
        regex: Regex::new(pat).expect("static secret-shape regex compiles"),
    };
    vec![
        // AWS access key ID.
        mk("aws-access-key", r"\bAKIA[0-9A-Z]{16}\b"),
        mk("aws-temp-access-key", r"\bASIA[0-9A-Z]{16}\b"),
        // Stripe live secret key (test keys sk_test_ are explicitly *not*
        // flagged — fixtures and docs use them all the time).
        mk("stripe-live-key", r"\bsk_live_[0-9a-zA-Z]{20,}\b"),
        mk(
            "stripe-restricted-live-key",
            r"\brk_live_[0-9a-zA-Z]{20,}\b",
        ),
        // GitHub tokens.
        mk("github-token", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
        // Slack tokens.
        mk("slack-token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b"),
        // OpenAI keys (legacy `sk-...` and `sk-proj-...`).
        mk("openai-key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}\b"),
        // Anthropic keys.
        mk("anthropic-key", r"\bsk-ant-[A-Za-z0-9_-]{20,}\b"),
        // Google API keys.
        mk("google-api-key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
        // PEM private keys.
        mk(
            "pem-private-key",
            r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY-----",
        ),
    ]
});

#[cfg(test)]
mod tests {
    use super::*;

    fn bf(rel: &str, content: &str) -> BundleFile {
        BundleFile {
            path: std::path::PathBuf::from(rel),
            relative_path: rel.to_string(),
            content: content.as_bytes().to_vec(),
            hash: "h".to_string(),
            size: content.len() as u64,
        }
    }

    fn br(rel: &str, content: &str) -> BundleResource {
        BundleResource {
            rel_path: rel.to_string(),
            abs_path: std::path::PathBuf::from(rel),
            content: content.as_bytes().to_vec(),
            hash: "h".to_string(),
            size: content.len() as u64,
        }
    }

    #[test]
    fn detects_aws_access_key_in_source() {
        let files = vec![bf(
            "hot/src/leak.hot",
            "key \"AKIAIOSFODNN7EXAMPLE\" // bad",
        )];
        let hits = scan_bundle(&files, &[], &SecretScanOpts::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "aws-access-key");
        assert_eq!(hits[0].path, "hot/src/leak.hot");
        assert!(
            !hits[0].snippet.contains("AKIAIOSFODNN7EXAMPLE"),
            "snippet must redact the secret"
        );
    }

    #[test]
    fn detects_secret_in_resource_under_prefixed_path() {
        let resources = vec![br(
            "config/prod.json",
            "{\"openai_api_key\": \"sk-proj-aaaabbbbccccddddeeeeffffgggghhhh1234\"}",
        )];
        let hits = scan_bundle(&[], &resources, &SecretScanOpts::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "openai-key");
        assert_eq!(hits[0].path, "resources/config/prod.json");
    }

    #[test]
    fn allowlist_glob_skips_resource() {
        let resources = vec![br(
            "fixtures/sample.json",
            "{\"key\": \"AKIAIOSFODNN7EXAMPLE\"}",
        )];
        let opts = SecretScanOpts::new(vec!["resources/fixtures/*.json".to_string()], false);
        let hits = scan_bundle(&[], &resources, &opts).unwrap();
        assert!(hits.is_empty(), "{:?}", hits);
    }

    #[test]
    fn allow_all_disables_scan() {
        let files = vec![bf("a.hot", "AKIAIOSFODNN7EXAMPLE")];
        let opts = SecretScanOpts::new(vec![], true);
        let hits = scan_bundle(&files, &[], &opts).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn does_not_flag_stripe_test_keys() {
        let files = vec![bf("hot/src/x.hot", "sk_test_abcdefghijklmnopqrstuvwx")];
        let hits = scan_bundle(&files, &[], &SecretScanOpts::default()).unwrap();
        assert!(hits.is_empty(), "{:?}", hits);
    }

    #[test]
    fn detects_pem_private_key() {
        let resources = vec![br(
            "keys/id_rsa",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBA...\n-----END RSA PRIVATE KEY-----",
        )];
        let hits = scan_bundle(&[], &resources, &SecretScanOpts::default()).unwrap();
        assert!(
            hits.iter().any(|h| h.kind == "pem-private-key"),
            "{:?}",
            hits
        );
    }

    #[test]
    fn skips_binary_files() {
        let resources = vec![BundleResource {
            rel_path: "blob.bin".into(),
            abs_path: std::path::PathBuf::from("blob.bin"),
            // Invalid UTF-8 sequence.
            content: vec![0xff, 0xfe, 0xfd, 0x00],
            hash: "h".into(),
            size: 4,
        }];
        let hits = scan_bundle(&[], &resources, &SecretScanOpts::default()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn results_sorted_by_path_then_line() {
        let files = vec![
            bf(
                "z.hot",
                "first\nAKIAIOSFODNN7EXAMPLE\nthird\nAKIA1234567890ABCDEF",
            ),
            bf("a.hot", "AKIA1234567890ABCDEF"),
        ];
        let hits = scan_bundle(&files, &[], &SecretScanOpts::default()).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].path, "a.hot");
        assert_eq!(hits[1].path, "z.hot");
        assert_eq!(hits[1].line, 2);
        assert_eq!(hits[2].line, 4);
    }

    #[test]
    fn negation_in_allowlist_re_includes_specific_file() {
        // Demonstrates the standard gitignore semantics: broad allow + a
        // negation that pulls one file back into the scan. Useful when
        // most fixtures are noise but one needs to be checked.
        let resources = vec![
            br("fixtures/ok.json", "{\"key\": \"AKIAIOSFODNN7EXAMPLE\"}"),
            br(
                "fixtures/check-me.json",
                "{\"key\": \"AKIAIOSFODNN7EXAMPLE\"}",
            ),
        ];
        let opts = SecretScanOpts::new(
            vec![
                "resources/fixtures/*.json".to_string(),
                "!resources/fixtures/check-me.json".to_string(),
            ],
            false,
        );
        let hits = scan_bundle(&[], &resources, &opts).unwrap();
        assert_eq!(hits.len(), 1, "{:?}", hits);
        assert_eq!(hits[0].path, "resources/fixtures/check-me.json");
    }

    #[test]
    fn format_findings_includes_paths_and_kinds() {
        let hits = vec![SecretMatch {
            path: "a.hot".into(),
            kind: "aws-access-key",
            line: 3,
            snippet: "key \"AKIA***MPLE\"".into(),
        }];
        let msg = format_findings(&hits);
        assert!(msg.contains("a.hot"));
        assert!(msg.contains("aws-access-key"));
        assert!(msg.contains("--allow-secret-shape"));
    }
}
