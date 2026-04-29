//! Handle/slug validation shared across signup, claim-handle, and orgs-new.
//!
//! Centralizes:
//! - Format rules (lowercase ASCII + digits + hyphens, length 2..=32)
//! - Reserved word list (protects top-level routes, impersonation-prone names)
//! - Availability check across existing orgs + non-expired pending verifications
//! - Alternative-slug suggestions on conflict (`alice` → `alice-2`, `alice-3`, …)

use hot::db::DatabasePool;

/// Reserved slugs that users are not allowed to claim.
///
/// Covers:
/// - Top-level app routes (so `/@admin` and `/admin` don't feel ambiguous)
/// - Common impersonation/phishing names
/// - The company brand
/// - Small set of common system paths
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
    // ── Brand / product names (us) ──────────────────────────────────────────
    "hot",
    "hotdev",
    "hot-dev",
    "hotcloud",
    "hot-cloud",
    "hot-cloud-free",
    "hot-cloud-starter",
    "hot-cloud-pro",
    "hot-cloud-scale",
    "hot-free",
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
/// Callers should call [`ensure_available`] afterward for the DB checks.
pub fn validate_format(slug: &str) -> Result<(), SlugError> {
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
    if is_reserved(slug) {
        return Err(SlugError::Reserved);
    }
    Ok(())
}

/// True if `slug` is in the reserved list.
pub fn is_reserved(slug: &str) -> bool {
    RESERVED_SLUGS.iter().any(|r| r.eq_ignore_ascii_case(slug))
}

/// Is this slug currently available? Checks:
/// - existing orgs (hard conflict)
/// - non-expired pending email verifications (soft reservation)
///
/// Assumes [`validate_format`] already passed.
pub async fn is_available(db: &DatabasePool, slug: &str) -> bool {
    if hot::db::org::Org::get_org_by_slug(db, slug).await.is_ok() {
        return false;
    }
    if hot::db::EmailVerification::has_pending_slug(db, slug)
        .await
        .unwrap_or(false)
    {
        return false;
    }
    true
}

/// Full validation: format + reserved + availability.
pub async fn ensure_available(db: &DatabasePool, slug: &str) -> Result<(), SlugError> {
    validate_format(slug)?;
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
        assert_eq!(validate_format("hot"), Err(SlugError::Reserved));
        assert_eq!(validate_format("hot-dev"), Err(SlugError::Reserved));
        assert_eq!(validate_format("hotdev"), Err(SlugError::Reserved));
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
}
