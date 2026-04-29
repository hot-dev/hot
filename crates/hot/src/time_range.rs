//! ISO 8601 duration parser used to validate `time_range` query parameters.
//!
//! Why this exists
//! ===============
//!
//! Several handlers accept a `time_range` query string (e.g. `P7D`, `P30D`)
//! and forward it to the database layer to filter results. Historically the
//! string was passed through verbatim, which meant:
//!
//!   1. The literal `"all"` (which the UI emits to mean "no filter") reached
//!      Postgres as `'all'::interval` and produced a 500.
//!   2. Any other invalid string was either silently coerced to a default
//!      (SQLite `task` path) or surfaced as a Postgres `invalid input syntax`
//!      error.
//!   3. Postgres and SQLite paths interpreted the same input differently.
//!
//! `parse_time_range` is the single boundary check: it accepts the
//! UX-special `"all"` (and `None`/empty) as "no filter", parses anything
//! else as a strict ISO 8601 duration, and bounds the result so a client
//! cannot ask for `P10000Y` worth of history. Callers convert the returned
//! [`TimeRange`] into a `DateTime<Utc>` cutoff via [`TimeRange::cutoff`] and
//! bind that to the query — no interval string ever reaches SQL.
//!
//! Note: SQL injection was never a risk here (all values were parameter-
//! bound). This module is about *validation*, not *escaping*.

use chrono::{DateTime, Datelike, Months, Utc};

/// Maximum total duration we'll accept, expressed in days. Prevents a
/// client from requesting `P10000Y` and forcing the DB to scan everything.
/// 10 calendar years is far longer than any realistic dashboard view.
pub const MAX_DAYS: u64 = 10 * 366;

/// A parsed, validated ISO 8601 duration.
///
/// All fields are non-negative integers. Fractional values and negative
/// durations are rejected at parse time. `weeks` is preserved separately
/// from `days` for fidelity but is treated as `7 * weeks` days when
/// computing the cutoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeRange {
    pub years: u32,
    pub months: u32,
    pub weeks: u32,
    pub days: u32,
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRangeError {
    /// Empty or missing payload after the leading `P`.
    Empty,
    /// Did not start with `P`.
    MissingDesignator,
    /// A number ran without a unit suffix, an unknown unit appeared, or
    /// the date/time sections were in the wrong order.
    InvalidSyntax,
    /// Numeric overflow while parsing or summing components.
    Overflow,
    /// Parsed successfully but exceeds [`MAX_DAYS`] or is exactly zero.
    OutOfBounds,
}

impl TimeRange {
    /// Parse a strict ISO 8601 duration string of the form
    /// `P[nY][nM][nW][nD][T[nH][nM][nS]]`. Fractional components and the
    /// week designator combined with other date fields are accepted (the
    /// spec allows the week form on its own, but most ISO 8601 parsers in
    /// the wild — including Postgres — happily mix them, and the cutoff
    /// math doesn't care).
    ///
    /// The string must contain at least one component; bare `P` and `PT`
    /// are rejected as [`TimeRangeError::Empty`].
    pub fn parse(input: &str) -> Result<Self, TimeRangeError> {
        // The grammar is small enough that a hand-rolled scanner is cleaner
        // than pulling in a parser combinator. We walk the string once,
        // accumulating digits into a number until we hit a designator, at
        // which point we slot the number into the matching field and zero
        // the accumulator.
        let bytes = input.as_bytes();
        if bytes.first() != Some(&b'P') {
            return Err(TimeRangeError::MissingDesignator);
        }

        let mut out = TimeRange::default();
        let mut in_time_section = false;
        let mut current: u64 = 0;
        let mut have_digits = false;
        let mut have_any_component = false;

        for &b in &bytes[1..] {
            match b {
                b'T' => {
                    if in_time_section || have_digits {
                        // `T` after we've already entered the time section,
                        // or `T` immediately following a number with no
                        // unit, is malformed.
                        return Err(TimeRangeError::InvalidSyntax);
                    }
                    in_time_section = true;
                }
                b'0'..=b'9' => {
                    let digit = (b - b'0') as u64;
                    current = current
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(digit))
                        .ok_or(TimeRangeError::Overflow)?;
                    have_digits = true;
                }
                designator => {
                    if !have_digits {
                        // A unit letter with no preceding number, e.g. `PD`.
                        return Err(TimeRangeError::InvalidSyntax);
                    }
                    let value: u32 =
                        u32::try_from(current).map_err(|_| TimeRangeError::Overflow)?;
                    match (in_time_section, designator) {
                        // Date section.
                        (false, b'Y') => out.years = out.years.saturating_add(value),
                        (false, b'M') => out.months = out.months.saturating_add(value),
                        (false, b'W') => out.weeks = out.weeks.saturating_add(value),
                        (false, b'D') => out.days = out.days.saturating_add(value),
                        // Time section.
                        (true, b'H') => out.hours = out.hours.saturating_add(value),
                        (true, b'M') => out.minutes = out.minutes.saturating_add(value),
                        (true, b'S') => out.seconds = out.seconds.saturating_add(value),
                        _ => return Err(TimeRangeError::InvalidSyntax),
                    }
                    current = 0;
                    have_digits = false;
                    have_any_component = true;
                }
            }
        }

        if have_digits {
            // Trailing digits with no unit, e.g. `P30`.
            return Err(TimeRangeError::InvalidSyntax);
        }
        if !have_any_component {
            // Just `P` or `PT` with nothing else.
            return Err(TimeRangeError::Empty);
        }

        if !out.within_bounds() {
            return Err(TimeRangeError::OutOfBounds);
        }
        Ok(out)
    }

    /// True if the duration is non-zero and below [`MAX_DAYS`].
    ///
    /// Months and years use a coarse "max-length" approximation (31 days
    /// per month, 366 per year) so the bound is conservative — callers
    /// using calendar-aware subtraction in [`Self::cutoff`] cannot land
    /// further back than this estimate. The total is computed in seconds
    /// so that sub-day components (a `PT5M` request, for example) are
    /// recognised as non-zero.
    pub fn within_bounds(&self) -> bool {
        const SEC_PER_DAY: u64 = 86_400;
        const MAX_SECONDS: u64 = MAX_DAYS * SEC_PER_DAY;
        let total = (self.years as u64).saturating_mul(366 * SEC_PER_DAY)
            + (self.months as u64).saturating_mul(31 * SEC_PER_DAY)
            + (self.weeks as u64).saturating_mul(7 * SEC_PER_DAY)
            + (self.days as u64).saturating_mul(SEC_PER_DAY)
            + (self.hours as u64).saturating_mul(3_600)
            + (self.minutes as u64).saturating_mul(60)
            + (self.seconds as u64);
        total > 0 && total <= MAX_SECONDS
    }

    /// Compute the cutoff timestamp for this duration relative to `now`,
    /// using calendar-aware subtraction for months and years (so `P1M`
    /// from March 31 lands on the last day of February, not "30 days ago").
    ///
    /// Returns `now` itself if subtraction would underflow the chrono
    /// representable range, which is effectively impossible given
    /// [`MAX_DAYS`] but is handled defensively.
    pub fn cutoff(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        // Calendar-aware months/years first; chrono's `Months::new` wraps
        // both into one operation that respects month length.
        let total_months = self.years.saturating_mul(12).saturating_add(self.months);
        let mut t = if total_months > 0 {
            now.checked_sub_months(Months::new(total_months))
                .unwrap_or(now)
        } else {
            now
        };

        // Then fixed-length components.
        let fixed_seconds: i64 = (self.weeks as i64) * 7 * 86_400
            + (self.days as i64) * 86_400
            + (self.hours as i64) * 3_600
            + (self.minutes as i64) * 60
            + (self.seconds as i64);
        if fixed_seconds > 0
            && let Some(d) = chrono::Duration::try_seconds(fixed_seconds)
        {
            t = t.checked_sub_signed(d).unwrap_or(t);
        }

        // Defensive: if the calendar math somehow produced a year 0/etc.,
        // pin to `now` rather than returning a nonsensical cutoff.
        if t.year() < 1 {
            return now;
        }
        t
    }
}

/// Parse the `time_range` query parameter into an optional [`TimeRange`].
///
/// `None`, `Some("")`, and `Some("all")` all map to `None` ("no filter").
/// Any other value is parsed strictly; unparseable or out-of-bounds inputs
/// also map to `None` so handlers can treat invalid input the same as
/// "all" instead of returning a 500.
///
/// Handlers that want to distinguish "invalid input" from "all" should
/// call [`TimeRange::parse`] directly and respond with `400 Bad Request`.
pub fn parse_time_range(raw: Option<&str>) -> Option<TimeRange> {
    match raw {
        None | Some("") | Some("all") => None,
        Some(s) => TimeRange::parse(s).ok(),
    }
}

/// Like [`parse_time_range`] but returns the cutoff timestamp directly.
/// This is the form most DB call sites want.
pub fn parse_time_range_cutoff(raw: Option<&str>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    parse_time_range(raw).map(|r| r.cutoff(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_common_dropdown_values() {
        for (input, expected) in [
            (
                "P1D",
                TimeRange {
                    days: 1,
                    ..Default::default()
                },
            ),
            (
                "P7D",
                TimeRange {
                    days: 7,
                    ..Default::default()
                },
            ),
            (
                "P30D",
                TimeRange {
                    days: 30,
                    ..Default::default()
                },
            ),
            (
                "P90D",
                TimeRange {
                    days: 90,
                    ..Default::default()
                },
            ),
            (
                "PT24H",
                TimeRange {
                    hours: 24,
                    ..Default::default()
                },
            ),
            (
                "P1M",
                TimeRange {
                    months: 1,
                    ..Default::default()
                },
            ),
            (
                "P1Y",
                TimeRange {
                    years: 1,
                    ..Default::default()
                },
            ),
        ] {
            assert_eq!(
                TimeRange::parse(input).ok(),
                Some(expected),
                "input={input}"
            );
        }
    }

    #[test]
    fn parses_combined_components() {
        assert_eq!(
            TimeRange::parse("P1Y2M3DT4H5M6S").ok(),
            Some(TimeRange {
                years: 1,
                months: 2,
                days: 3,
                hours: 4,
                minutes: 5,
                seconds: 6,
                ..Default::default()
            })
        );
    }

    #[test]
    fn distinguishes_date_minutes_from_time_minutes() {
        // `M` in date section = months, `M` in time section = minutes.
        assert_eq!(
            TimeRange::parse("P5M").ok(),
            Some(TimeRange {
                months: 5,
                ..Default::default()
            })
        );
        assert_eq!(
            TimeRange::parse("PT5M").ok(),
            Some(TimeRange {
                minutes: 5,
                ..Default::default()
            })
        );
    }

    #[test]
    fn rejects_invalid_inputs() {
        let cases = [
            ("", TimeRangeError::MissingDesignator),
            ("7D", TimeRangeError::MissingDesignator),
            ("P", TimeRangeError::Empty),
            ("PT", TimeRangeError::Empty),
            ("P30", TimeRangeError::InvalidSyntax),
            ("PD", TimeRangeError::InvalidSyntax),
            ("PTD", TimeRangeError::InvalidSyntax),
            ("P1H", TimeRangeError::InvalidSyntax), // hours only valid after T
            ("P1S", TimeRangeError::InvalidSyntax),
            ("PT1Y", TimeRangeError::InvalidSyntax), // years not valid after T
            ("PT1D", TimeRangeError::InvalidSyntax),
            ("PTT1H", TimeRangeError::InvalidSyntax),
            ("P1.5D", TimeRangeError::InvalidSyntax),
            ("P-1D", TimeRangeError::InvalidSyntax),
            ("P 1D", TimeRangeError::InvalidSyntax),
            ("p1d", TimeRangeError::MissingDesignator), // case-sensitive
        ];
        for (input, expected) in cases {
            assert_eq!(TimeRange::parse(input), Err(expected), "input={input}");
        }
    }

    #[test]
    fn rejects_zero_duration() {
        assert_eq!(TimeRange::parse("P0D"), Err(TimeRangeError::OutOfBounds));
        assert_eq!(
            TimeRange::parse("PT0H0M0S"),
            Err(TimeRangeError::OutOfBounds)
        );
    }

    #[test]
    fn rejects_out_of_bounds() {
        assert_eq!(
            TimeRange::parse("P10000Y"),
            Err(TimeRangeError::OutOfBounds)
        );
        assert_eq!(TimeRange::parse("P11Y"), Err(TimeRangeError::OutOfBounds));
        // Right at the edge:
        assert!(TimeRange::parse("P10Y").is_ok());
    }

    #[test]
    fn cutoff_subtracts_days() {
        let r = TimeRange::parse("P7D").unwrap();
        let expected = now() - chrono::Duration::days(7);
        assert_eq!(r.cutoff(now()), expected);
    }

    #[test]
    fn cutoff_uses_calendar_months() {
        // March 31, 2026 - 1 month = February 28, 2026 (chrono clamps to
        // the last valid day of the target month).
        let n = Utc.with_ymd_and_hms(2026, 3, 31, 12, 0, 0).unwrap();
        let r = TimeRange::parse("P1M").unwrap();
        let cutoff = r.cutoff(n);
        assert_eq!(cutoff.month(), 2);
        assert_eq!(cutoff.day(), 28);
    }

    #[test]
    fn parse_time_range_handles_special_inputs() {
        assert_eq!(parse_time_range(None), None);
        assert_eq!(parse_time_range(Some("")), None);
        assert_eq!(parse_time_range(Some("all")), None);
        assert_eq!(parse_time_range(Some("garbage")), None);
        assert_eq!(parse_time_range(Some("P10000Y")), None);
        assert!(parse_time_range(Some("P7D")).is_some());
    }

    #[test]
    fn cutoff_helper_returns_none_for_all() {
        assert_eq!(parse_time_range_cutoff(Some("all"), now()), None);
        assert_eq!(
            parse_time_range_cutoff(Some("P1D"), now()),
            Some(now() - chrono::Duration::days(1))
        );
    }
}
