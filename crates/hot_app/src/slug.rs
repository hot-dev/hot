//! Handle/slug validation shared across signup, claim-handle, and orgs-new.
//!
//! Centralizes:
//! - Format rules (lowercase ASCII + digits + hyphens, length 2..=32)
//! - Reserved word list (protects top-level routes, impersonation-prone names)
//! - Deployment-supplied reserved list via the `hot.org.reserved-slugs` Hot
//!   conf value (additive to the baked-in [`RESERVED_SLUGS`])
//! - Availability check across existing orgs + non-expired pending verifications
//! - Alternative-slug suggestions on conflict (`alice` → `alice-2`, `alice-3`, …)

use hot::db::DatabasePool;
use hot::val::Val;

/// Reserved slugs that users are not allowed to claim.
///
/// Covers protections that apply to every Hot deployment regardless of
/// operator:
/// - Top-level app routes (so `/@admin` and `/admin` don't feel ambiguous)
/// - Common impersonation/phishing names
/// - Small set of common system paths
///
/// Operator-/deployment-specific protections (the operator's own brand,
/// founder/staff handles, paid-tier names, etc.) belong in
/// `hot.org.reserved-slugs` instead — see [`extra_reserved_from_conf`]. Hot
/// Cloud's own brand list lives in `hot-cloud/aws/ecs/app.hot` for that
/// reason; do not re-add those entries here.
pub const RESERVED_SLUGS: &[&str] = &[
    // ── App routes & namespaces ─────────────────────────────────────────────
    "admin",
    "api",
    "app",
    "auth",
    "billing",
    "cancel",
    "checkout",
    "claim-handle",
    "dashboard",
    "data",
    "docs",
    "documentation",
    "env",
    "help",
    "invite",
    "invites",
    "me",
    "new",
    "oauth",
    "orgs",
    "organizations",
    "pricing",
    "profile",
    "runs",
    "settings",
    "signin",
    "signout",
    "signup",
    "status",
    "support",
    "team",
    "teams",
    "users",
    "verify",
    "webhook",
    "webhooks",
    // ── Impersonation-prone / authority-sounding ────────────────────────────
    "official",
    "root",
    "staff",
    "system",
    "superuser",
    "moderator",
    "mod",
    "anonymous",
    "guest",
    "user",
    "account",
    "owner",
    // ── Generic business / corporate placeholders ───────────────────────────
    "inc",
    "llc",
    "corp",
    "ltd",
    "co",
    "company",
    "gmbh",
    "enterprise",
    "enterprises",
    "holdings",
    "group",
    "acme",
    "example",
    "test",
    "testing",
    "demo",
    "sample",
    "foo",
    "bar",
    "baz",
    // ── SaaS / plan terminology ─────────────────────────────────────────────
    "cli",
    "sdk",
    "cloud",
    "platform",
    "service",
    "services",
    "free",
    "pro",
    "starter",
    "scale",
    "trial",
    "paid",
    // ── Big-tech / impersonation-prone brands ───────────────────────────────
    "google",
    "alphabet",
    "apple",
    "microsoft",
    "msft",
    "amazon",
    "aws",
    "meta",
    "facebook",
    "instagram",
    "whatsapp",
    "twitter",
    "openai",
    "anthropic",
    "claude",
    "chatgpt",
    "github",
    "gitlab",
    "bitbucket",
    "stripe",
    "vercel",
    "netlify",
    "cloudflare",
    "heroku",
    "linear",
    "notion",
    "figma",
    "slack",
    "discord",
    "zoom",
    "salesforce",
    "shopify",
    "square",
    // ── Common noise ────────────────────────────────────────────────────────
    "about",
    "blog",
    "contact",
    "home",
    "login",
    "logout",
    "null",
    "undefined",
    "www",
];

/// Result of a slug validation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugError {
    Empty,
    TooShort,
    TooLong,
    InvalidFormat,
    Reserved,
    Taken,
}

impl SlugError {
    /// User-facing error message.
    pub fn message(&self) -> &'static str {
        match self {
            Self::Empty => "Handle is required",
            Self::TooShort => "Handle must be at least 2 characters",
            Self::TooLong => "Handle must be 32 characters or fewer",
            Self::InvalidFormat => {
                "Handle can only contain lowercase letters, numbers, and hyphens"
            }
            Self::Reserved => "This handle is reserved. Please choose a different one.",
            Self::Taken => "This handle is already taken. Please choose a different one.",
        }
    }
}

/// Validate slug format and reserved-word rules. Fast, no I/O.
///
/// Equivalent to [`validate_format_with_extra`] with no deployment extras.
/// Prefer the `_with_extra` variant from any handler that has a [`Val`] in
/// scope so the `hot.org.reserved-slugs` deployment list is honored.
///
/// Callers should call [`ensure_available`] afterward for the DB checks.
pub fn validate_format(slug: &str) -> Result<(), SlugError> {
    validate_format_with_extra(slug, &[])
}

/// Validate slug format and reserved-word rules, including a deployment-
/// supplied extra reserved list (see [`extra_reserved_from_conf`]). Fast, no I/O.
pub fn validate_format_with_extra(slug: &str, extra_reserved: &[String]) -> Result<(), SlugError> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err(SlugError::Empty);
    }
    if slug.len() < 2 {
        return Err(SlugError::TooShort);
    }
    if slug.len() > 32 {
        return Err(SlugError::TooLong);
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(SlugError::InvalidFormat);
    }
    // Disallow leading/trailing hyphens and double hyphens — common footguns.
    if slug.starts_with('-') || slug.ends_with('-') || slug.contains("--") {
        return Err(SlugError::InvalidFormat);
    }
    if is_reserved(slug) || is_extra_reserved(slug, extra_reserved) {
        return Err(SlugError::Reserved);
    }
    Ok(())
}

/// True if `slug` is in the baked-in [`RESERVED_SLUGS`] list.
pub fn is_reserved(slug: &str) -> bool {
    RESERVED_SLUGS.iter().any(|r| r.eq_ignore_ascii_case(slug))
}

/// True if `slug` matches any entry in `extra_reserved` (case-insensitive).
///
/// Use this together with [`is_reserved`] to honor a deployment-supplied
/// reserved list (e.g. founder names, paid-tier brand slugs that aren't in
/// the public open-source default).
pub fn is_extra_reserved(slug: &str, extra_reserved: &[String]) -> bool {
    extra_reserved.iter().any(|r| r.eq_ignore_ascii_case(slug))
}

/// Read the deployment-supplied reserved-slug list from Hot conf. Empty if
/// unset. Entries are normalized (trimmed + lowercased) and empty entries
/// dropped, so the conf author can be sloppy about whitespace and casing.
///
/// Set in conf as e.g. `hot.org.reserved-slugs ["curtis", "founder", …]`.
pub fn extra_reserved_from_conf(conf: &Val) -> Vec<String> {
    conf.get_vec_str_or_default("org.reserved-slugs", Vec::new())
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Is this slug currently available (no existing org owns it)?
///
/// Assumes [`validate_format`] already passed. Handles are claimed
/// post-verification, so there are no pending reservations to consider.
pub async fn is_available(db: &DatabasePool, slug: &str) -> bool {
    hot::db::org::Org::get_org_by_slug(db, slug).await.is_err()
}

/// Full validation: format + reserved + availability.
///
/// Equivalent to [`ensure_available_with_extra`] with no deployment extras.
pub async fn ensure_available(db: &DatabasePool, slug: &str) -> Result<(), SlugError> {
    ensure_available_with_extra(db, slug, &[]).await
}

/// Full validation: format + reserved (baked-in + deployment extras) + availability.
pub async fn ensure_available_with_extra(
    db: &DatabasePool,
    slug: &str,
    extra_reserved: &[String],
) -> Result<(), SlugError> {
    validate_format_with_extra(slug, extra_reserved)?;
    if !is_available(db, slug).await {
        return Err(SlugError::Taken);
    }
    Ok(())
}

/// Suggest the first available slug based on `base`. Tries `base`, `base-2`, … up to `-99`.
/// Falls back to the original base if nothing is found (extremely unlikely in practice).
pub async fn suggest_available(db: &DatabasePool, base: &str) -> String {
    let base = base.trim().trim_matches('-');
    // Ensure the base itself is format-valid before suggesting derivatives.
    // If not, return as-is so the caller surfaces the format error normally.
    if validate_format(base).is_err() && !is_reserved(base) {
        return base.to_string();
    }
    if !is_reserved(base) && is_available(db, base).await {
        return base.to_string();
    }
    suggest_alternative(db, base).await
}

/// Suggest an alternative slug DIFFERENT from `base`. Always tries `base-2`, `base-3`, …
/// Never returns `base` unchanged.
///
/// Use this from error paths where we KNOW `base` was just rejected — e.g. an
/// insert that failed with a unique-constraint violation — even if a subsequent
/// availability check (which may hit a stale read replica) says the slug is
/// free.
pub async fn suggest_alternative(db: &DatabasePool, base: &str) -> String {
    let base = base.trim().trim_matches('-');
    for n in 2..=99 {
        let candidate = format!("{}-{}", base, n);
        if candidate.len() > 32 {
            break;
        }
        if !is_reserved(&candidate) && is_available(db, &candidate).await {
            return candidate;
        }
    }
    // Extreme last resort: if `-2..=99` are all taken or over the length cap,
    // return `base-2` anyway so the user sees *something different* from what
    // they just tried.
    format!("{}-2", base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(validate_format(""), Err(SlugError::Empty));
        assert_eq!(validate_format("   "), Err(SlugError::Empty));
    }

    #[test]
    fn rejects_too_short() {
        assert_eq!(validate_format("a"), Err(SlugError::TooShort));
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(33);
        assert_eq!(validate_format(&long), Err(SlugError::TooLong));
    }

    #[test]
    fn rejects_invalid_chars() {
        assert_eq!(validate_format("Alice"), Err(SlugError::InvalidFormat));
        assert_eq!(validate_format("alice_bob"), Err(SlugError::InvalidFormat));
        assert_eq!(validate_format("alice.bob"), Err(SlugError::InvalidFormat));
    }

    #[test]
    fn rejects_leading_trailing_double_hyphens() {
        assert_eq!(validate_format("-alice"), Err(SlugError::InvalidFormat));
        assert_eq!(validate_format("alice-"), Err(SlugError::InvalidFormat));
        assert_eq!(validate_format("al--ice"), Err(SlugError::InvalidFormat));
    }

    #[test]
    fn rejects_reserved() {
        assert_eq!(validate_format("admin"), Err(SlugError::Reserved));
        assert_eq!(validate_format("ADMIN"), Err(SlugError::InvalidFormat)); // uppercase fails first
        assert_eq!(validate_format("api"), Err(SlugError::Reserved));
        assert_eq!(validate_format("acme"), Err(SlugError::Reserved));
        assert_eq!(validate_format("google"), Err(SlugError::Reserved));
        assert_eq!(validate_format("inc"), Err(SlugError::Reserved));
    }

    #[test]
    fn exact_reserved_only_not_substring() {
        // "acme" is reserved, but "acme-inc-42" is a valid composed slug.
        assert!(validate_format("acme-inc-42").is_ok());
        assert!(validate_format("my-google").is_ok());
    }

    #[test]
    fn accepts_valid() {
        assert!(validate_format("alice").is_ok());
        assert!(validate_format("alice-bob").is_ok());
        assert!(validate_format("a1").is_ok());
        assert!(validate_format("acme-inc-42").is_ok());
    }

    #[test]
    fn is_reserved_is_case_insensitive() {
        assert!(is_reserved("admin"));
        assert!(is_reserved("ADMIN"));
        assert!(is_reserved("Admin"));
        assert!(!is_reserved("alice"));
    }

    #[test]
    fn extra_reserved_blocks_validation() {
        let extra = vec!["curtis".to_string(), "founder".to_string()];

        // Baked-in reserved still fires.
        assert_eq!(
            validate_format_with_extra("admin", &extra),
            Err(SlugError::Reserved)
        );

        // Extra-reserved fires the same Reserved error so callers get a
        // consistent user-facing message regardless of which list matched.
        assert_eq!(
            validate_format_with_extra("curtis", &extra),
            Err(SlugError::Reserved)
        );
        assert_eq!(
            validate_format_with_extra("founder", &extra),
            Err(SlugError::Reserved)
        );

        // Non-matching slug still passes when extras are present.
        assert!(validate_format_with_extra("alice", &extra).is_ok());
    }

    #[test]
    fn extra_reserved_is_case_insensitive() {
        let extra = vec!["Curtis".to_string()];
        assert!(is_extra_reserved("curtis", &extra));
        assert!(is_extra_reserved("CURTIS", &extra));
        assert!(!is_extra_reserved("alice", &extra));
    }

    #[test]
    fn extra_reserved_only_matches_exact_slug_not_substring() {
        let extra = vec!["curtis".to_string()];
        assert!(validate_format_with_extra("curtis-co", &extra).is_ok());
        assert!(validate_format_with_extra("not-curtis", &extra).is_ok());
    }

    #[test]
    fn validate_format_without_extras_matches_old_behavior() {
        // The zero-extras shorthand must behave identically to the
        // direct-no-extras call; signup_flow.rs and other tests rely on this.
        assert_eq!(validate_format("admin"), Err(SlugError::Reserved));
        assert_eq!(
            validate_format_with_extra("admin", &[]),
            Err(SlugError::Reserved)
        );
        assert!(validate_format("alice").is_ok());
        assert!(validate_format_with_extra("alice", &[]).is_ok());
    }

    #[test]
    fn extra_reserved_from_conf_normalizes_and_drops_empty() {
        let conf = hot::val!({
            "org": {"reserved-slugs": ["  Curtis ", "FOUNDER", "", "   ", "ok"]},
        });
        let extras = extra_reserved_from_conf(&conf);
        assert_eq!(extras, vec!["curtis", "founder", "ok"]);
    }

    #[test]
    fn extra_reserved_from_conf_defaults_to_empty() {
        let conf = hot::val!({});
        assert!(extra_reserved_from_conf(&conf).is_empty());
    }
}
