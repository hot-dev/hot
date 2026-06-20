use uuid::Uuid;

/// Parsed user search input for UUID-backed list filters.
///
/// UI tables display UUIDs as the last 12 hyphenless hex characters. SQLite
/// stores UUIDs as blobs, while Postgres stores native UUIDs, so callers still
/// choose their dialect-specific SQL expression. This helper centralizes the
/// user-input classification and bind patterns so list and count queries stay
/// consistent.
#[derive(Debug, Clone)]
pub(crate) enum IdSearch {
    ExactUuid {
        uuid: Uuid,
        text_pattern: String,
    },
    ShortId {
        suffix_pattern: String,
        text_pattern: String,
    },
    Text {
        text_pattern: String,
    },
}

impl IdSearch {
    pub(crate) fn parse(term: Option<&str>) -> Option<Self> {
        let term = term?.trim();
        if term.is_empty() {
            return None;
        }

        if let Some(uuid) = parse_uuid(term) {
            return Some(Self::ExactUuid {
                uuid,
                text_pattern: contains_pattern(term),
            });
        }

        if is_short_uuid_id(term) {
            return Some(Self::ShortId {
                suffix_pattern: suffix_pattern(term),
                text_pattern: contains_pattern(term),
            });
        }

        Some(Self::Text {
            text_pattern: contains_pattern(term),
        })
    }

    pub(crate) fn uuid(&self) -> Option<Uuid> {
        match self {
            Self::ExactUuid { uuid, .. } => Some(*uuid),
            Self::ShortId { .. } | Self::Text { .. } => None,
        }
    }

    pub(crate) fn is_short_id(&self) -> bool {
        matches!(self, Self::ShortId { .. })
    }

    pub(crate) fn suffix_pattern(&self) -> Option<&str> {
        match self {
            Self::ShortId { suffix_pattern, .. } => Some(suffix_pattern),
            Self::ExactUuid { .. } | Self::Text { .. } => None,
        }
    }

    pub(crate) fn text_pattern(&self) -> &str {
        match self {
            Self::ExactUuid { text_pattern, .. }
            | Self::ShortId { text_pattern, .. }
            | Self::Text { text_pattern } => text_pattern,
        }
    }
}

fn parse_uuid(term: &str) -> Option<Uuid> {
    Uuid::parse_str(term).ok().or_else(|| {
        if term.len() == 32 && term.chars().all(|c| c.is_ascii_hexdigit()) {
            let with_dashes = format!(
                "{}-{}-{}-{}-{}",
                &term[0..8],
                &term[8..12],
                &term[12..16],
                &term[16..20],
                &term[20..32]
            );
            Uuid::parse_str(&with_dashes).ok()
        } else {
            None
        }
    })
}

fn is_short_uuid_id(term: &str) -> bool {
    term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit())
}

fn suffix_pattern(term: &str) -> String {
    format!("%{}", term)
}

fn contains_pattern(term: &str) -> String {
    format!("%{}%", term)
}

pub(crate) fn pg_placeholders(start: usize, len: usize) -> String {
    (start..start + len)
        .map(|i| format!("${}", i))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn sqlite_placeholders(len: usize) -> String {
    (0..len).map(|_| "?").collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn parse_none_and_blank_is_none() {
        assert!(IdSearch::parse(None).is_none());
        assert!(IdSearch::parse(Some("")).is_none());
        assert!(IdSearch::parse(Some("   ")).is_none());
    }

    #[test]
    fn parse_full_hyphenated_uuid() {
        let expected = Uuid::parse_str(UUID).unwrap();
        let search = IdSearch::parse(Some(UUID)).unwrap();

        assert_eq!(search.uuid(), Some(expected));
        assert!(!search.is_short_id());
        assert_eq!(search.suffix_pattern(), None);
        assert_eq!(search.text_pattern(), format!("%{UUID}%"));
    }

    #[test]
    fn parse_hyphenless_32_char_uuid() {
        let expected = Uuid::parse_str(UUID).unwrap();
        let search = IdSearch::parse(Some("550e8400e29b41d4a716446655440000")).unwrap();

        assert_eq!(search.uuid(), Some(expected));
        assert!(!search.is_short_id());
    }

    #[test]
    fn parse_uppercase_uuid_normalizes() {
        let expected = Uuid::parse_str(UUID).unwrap();
        let search = IdSearch::parse(Some("550E8400E29B41D4A716446655440000")).unwrap();

        assert_eq!(search.uuid(), Some(expected));
    }

    #[test]
    fn parse_short_id() {
        // The last 12 hyphenless hex chars, as shown in the UI.
        let search = IdSearch::parse(Some("446655440000")).unwrap();

        assert!(search.is_short_id());
        assert_eq!(search.uuid(), None);
        assert_eq!(search.suffix_pattern(), Some("%446655440000"));
        assert_eq!(search.text_pattern(), "%446655440000%");
    }

    #[test]
    fn parse_plain_text() {
        let search = IdSearch::parse(Some("my-fn")).unwrap();

        assert!(!search.is_short_id());
        assert_eq!(search.uuid(), None);
        assert_eq!(search.suffix_pattern(), None);
        assert_eq!(search.text_pattern(), "%my-fn%");
    }

    #[test]
    fn parse_twelve_non_hex_chars_is_text() {
        // Right length for a short id but not all hex, so treat as free text.
        let search = IdSearch::parse(Some("ghijklmnopqr")).unwrap();

        assert!(!search.is_short_id());
        assert_eq!(search.suffix_pattern(), None);
        assert_eq!(search.text_pattern(), "%ghijklmnopqr%");
    }

    #[test]
    fn parse_odd_length_hex_is_text() {
        // 13 hex chars: neither a full uuid (32) nor a short id (12).
        let search = IdSearch::parse(Some("0123456789abc")).unwrap();

        assert!(!search.is_short_id());
        assert_eq!(search.uuid(), None);
        assert_eq!(search.text_pattern(), "%0123456789abc%");
    }

    #[test]
    fn parse_trims_surrounding_whitespace() {
        let search = IdSearch::parse(Some("  446655440000  ")).unwrap();

        assert!(search.is_short_id());
        assert_eq!(search.suffix_pattern(), Some("%446655440000"));
    }

    #[test]
    fn pg_placeholders_format() {
        assert_eq!(pg_placeholders(1, 3), "$1, $2, $3");
        assert_eq!(pg_placeholders(5, 2), "$5, $6");
        assert_eq!(pg_placeholders(1, 0), "");
    }

    #[test]
    fn sqlite_placeholders_format() {
        assert_eq!(sqlite_placeholders(3), "?, ?, ?");
        assert_eq!(sqlite_placeholders(1), "?");
        assert_eq!(sqlite_placeholders(0), "");
    }
}
