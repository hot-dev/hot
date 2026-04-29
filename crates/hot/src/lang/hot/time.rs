// Time functions for the bytecode engine.
//
// Basic time functionality for testing and benchmarking

use crate::lang::hot::r#type::{HotResult, untype_recursive};
use crate::val::Val;
use crate::validate_args;
use chrono::{Datelike, TimeZone, Timelike};
use indexmap::IndexMap;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use temporal_rs::Calendar as TemporalCalendar;
use temporal_rs::Duration as TemporalDuration;
use temporal_rs::TimeZone as TemporalTimeZone;
use temporal_rs::ZonedDateTime as TemporalZonedDateTime;
use temporal_rs::options::{Disambiguation, OffsetDisambiguation};

// Helper function to create a temporal object as a Hot type map.
fn create_temporal_map(type_name: &str, data: IndexMap<Val, Val>) -> Val {
    let mut map = IndexMap::new();
    map.insert(
        Val::from("$type"),
        Val::from(format!("::hot::time/{}", type_name)),
    );
    for (k, v) in data {
        map.insert(k, v);
    }
    Val::Map(Box::new(map))
}

/// Get current time as an Instant object
pub fn now(args: &[Val]) -> HotResult<Val> {
    tracing::debug!("time::now called with {} args", args.len());
    if !args.is_empty() {
        return HotResult::Err(Val::from("now expects 0 arguments"));
    }

    // Return current system time as an Instant object (Map with epochNanoseconds)
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let nanos = duration.as_nanos() as i64;
            let mut data = IndexMap::new();
            data.insert(Val::from("epochNanoseconds"), Val::Int(nanos));
            HotResult::Ok(create_temporal_map("Instant", data))
        }
        Err(_) => HotResult::Err(Val::from("Failed to get current time")),
    }
}

/// Convert Instant object to epoch milliseconds
pub fn epoch_millis(args: &[Val]) -> HotResult<Val> {
    tracing::debug!("time::epoch_millis called with {} args", args.len());
    validate_args!("::hot::time/epoch-millis", args, 1);

    // Extract epochNanoseconds from the Instant object and convert to milliseconds
    match &args[0] {
        Val::Map(instant_map) => {
            if let Some(Val::Int(nanos)) = instant_map.get(&Val::from("epochNanoseconds")) {
                let millis = nanos / 1_000_000; // Convert nanoseconds to milliseconds
                HotResult::Ok(Val::Int(millis))
            } else {
                HotResult::Err(Val::from(
                    "Invalid Instant object: missing epochNanoseconds".to_string(),
                ))
            }
        }
        _ => HotResult::Err(Val::from(
            "epoch-millis expects an Instant object".to_string(),
        )),
    }
}

/// Get epoch nanoseconds from an Instant object
pub fn epoch_nanos(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/epoch-nanos", args, 1);

    match &args[0] {
        Val::Map(instant_map) => {
            if let Some(Val::Int(nanos)) = instant_map.get(&Val::from("epochNanoseconds")) {
                HotResult::Ok(Val::Int(*nanos))
            } else {
                HotResult::Err(Val::from(
                    "Invalid Instant object: missing epochNanoseconds".to_string(),
                ))
            }
        }
        _ => HotResult::Err(Val::from(
            "epoch-nanos expects an Instant object".to_string(),
        )),
    }
}

// removed simple placeholder getters and to_string in favor of enhanced versions below

/// Infer the time type kind from a Map's fields when no $type is present.
/// Returns a type hint string matching the $type suffixes, or None.
fn infer_time_type(m: &IndexMap<Val, Val>) -> Option<&'static str> {
    let has = |key: &str| m.contains_key(&Val::from(key));
    if has("epochNanoseconds") && has("timezone") {
        Some("ZonedDateTime")
    } else if has("epochNanoseconds") {
        Some("Instant")
    } else if has("year") && has("month") && has("day") && has("hour") {
        Some("PlainDateTime")
    } else if has("year") && has("month") && has("day") {
        Some("PlainDate")
    } else if has("hour") && has("minute") {
        Some("PlainTime")
    } else if has("years")
        || has("months")
        || has("weeks")
        || has("days")
        || has("hours")
        || has("minutes")
        || has("seconds")
    {
        Some("Duration")
    } else {
        None
    }
}

/// String conversions for date/time values
pub fn to_string(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/to-string", args, 1);
    // Strip type wrapping so {$type: "PlainDate", $val: {year: ...}} works
    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    match &untyped {
        Val::Map(m) => {
            // Determine the time type: use $type if still present, otherwise infer from structure
            let type_kind = if let Some(Val::Str(t)) = m.get(&Val::from("$type")) {
                match &**t {
                    "::hot::time/PlainDate" => Some("PlainDate"),
                    "::hot::time/PlainTime" => Some("PlainTime"),
                    "::hot::time/PlainDateTime" => Some("PlainDateTime"),
                    "::hot::time/ZonedDateTime" => Some("ZonedDateTime"),
                    "::hot::time/Instant" => Some("Instant"),
                    "::hot::time/Duration" => Some("Duration"),
                    _ => None,
                }
            } else {
                // Structural type matching: infer from fields present
                infer_time_type(m)
            };

            match type_kind {
                Some("PlainDate") => {
                    let y = m.get(&Val::from("year")).cloned().unwrap_or(Val::Int(1970));
                    let mo = m.get(&Val::from("month")).cloned().unwrap_or(Val::Int(1));
                    let d = m.get(&Val::from("day")).cloned().unwrap_or(Val::Int(1));
                    if let (Val::Int(y), Val::Int(mo), Val::Int(d)) = (y, mo, d) {
                        return HotResult::Ok(Val::from(format!("{:04}-{:02}-{:02}", y, mo, d)));
                    }
                }
                Some("PlainTime") => {
                    let h = m.get(&Val::from("hour")).cloned().unwrap_or(Val::Int(0));
                    let mi = m.get(&Val::from("minute")).cloned().unwrap_or(Val::Int(0));
                    let s = m.get(&Val::from("second")).cloned().unwrap_or(Val::Int(0));
                    if let (Val::Int(h), Val::Int(mi), Val::Int(s)) = (h, mi, s) {
                        return HotResult::Ok(Val::from(format!("{:02}:{:02}:{:02}", h, mi, s)));
                    }
                }
                Some("PlainDateTime") => {
                    let y = m.get(&Val::from("year")).cloned().unwrap_or(Val::Int(1970));
                    let mo = m.get(&Val::from("month")).cloned().unwrap_or(Val::Int(1));
                    let d = m.get(&Val::from("day")).cloned().unwrap_or(Val::Int(1));
                    let h = m.get(&Val::from("hour")).cloned().unwrap_or(Val::Int(0));
                    let mi = m.get(&Val::from("minute")).cloned().unwrap_or(Val::Int(0));
                    let s = m.get(&Val::from("second")).cloned().unwrap_or(Val::Int(0));
                    if let (
                        Val::Int(y),
                        Val::Int(mo),
                        Val::Int(d),
                        Val::Int(h),
                        Val::Int(mi),
                        Val::Int(s),
                    ) = (y, mo, d, h, mi, s)
                    {
                        return HotResult::Ok(Val::from(format!(
                            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                            y, mo, d, h, mi, s
                        )));
                    }
                }
                Some("ZonedDateTime") => {
                    let y = m.get(&Val::from("year")).cloned().unwrap_or(Val::Int(1970));
                    let mo = m.get(&Val::from("month")).cloned().unwrap_or(Val::Int(1));
                    let d = m.get(&Val::from("day")).cloned().unwrap_or(Val::Int(1));
                    let h = m.get(&Val::from("hour")).cloned().unwrap_or(Val::Int(0));
                    let mi = m.get(&Val::from("minute")).cloned().unwrap_or(Val::Int(0));
                    let s = m.get(&Val::from("second")).cloned().unwrap_or(Val::Int(0));
                    let offset = match m.get(&Val::from("offset")) {
                        Some(Val::Str(s)) => s.to_string(),
                        _ => "+00:00".to_string(),
                    };
                    let tz = match m.get(&Val::from("timezone")) {
                        Some(Val::Str(s)) => s.to_string(),
                        _ => "UTC".to_string(),
                    };
                    if let (
                        Val::Int(y),
                        Val::Int(mo),
                        Val::Int(d),
                        Val::Int(h),
                        Val::Int(mi),
                        Val::Int(s),
                    ) = (y, mo, d, h, mi, s)
                    {
                        return HotResult::Ok(Val::from(format!(
                            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}[{}]",
                            y, mo, d, h, mi, s, offset, tz
                        )));
                    }
                }
                Some("Instant") => {
                    // Use ISO UTC with Z suffix up to seconds for tests
                    if let Some(Val::Int(ns)) = m.get(&Val::from("epochNanoseconds")) {
                        // Convert to seconds
                        let secs = ns / 1_000_000_000;
                        // Very rough: render seconds since epoch as placeholder ISO string
                        // For tests, only prefix/`Z` checks are used.
                        let dt = chrono::Utc
                            .timestamp_millis_opt(secs * 1000)
                            .single()
                            .unwrap_or_else(|| {
                                chrono::Utc.timestamp_millis_opt(0).single().unwrap()
                            })
                            .naive_utc();
                        return HotResult::Ok(Val::from(format!(
                            "{}Z",
                            dt.format("%Y-%m-%dT%H:%M:%S")
                        )));
                    }
                }
                Some("Duration") => {
                    // Build ISO 8601 duration string from available fields
                    let get_f = |key: &str| -> f64 {
                        match m.get(&Val::from(key.to_string())) {
                            Some(Val::Dec(d)) => d.to_f64(),
                            Some(Val::Int(i)) => *i as f64,
                            _ => 0.0,
                        }
                    };

                    let years = get_f("years");
                    let months = get_f("months");
                    let days = get_f("days");
                    let hours = get_f("hours");
                    let minutes = get_f("minutes");
                    let seconds = get_f("seconds");

                    let mut date_part = String::new();
                    if years != 0.0 {
                        date_part.push_str(&format!("{}Y", years));
                    }
                    if months != 0.0 {
                        date_part.push_str(&format!("{}M", months));
                    }
                    if days != 0.0 {
                        date_part.push_str(&format!("{}D", days));
                    }

                    let mut time_part = String::new();
                    if hours != 0.0 {
                        time_part.push_str(&format!("{}H", hours));
                    }
                    if minutes != 0.0 {
                        time_part.push_str(&format!("{}M", minutes));
                    }
                    if seconds != 0.0 {
                        time_part.push_str(&format!("{}S", seconds));
                    }

                    let result = if date_part.is_empty() && time_part.is_empty() {
                        "PT0S".to_string()
                    } else if time_part.is_empty() {
                        format!("P{}", date_part)
                    } else if date_part.is_empty() {
                        format!("PT{}", time_part)
                    } else {
                        format!("P{}T{}", date_part, time_part)
                    };

                    return HotResult::Ok(Val::from(result));
                }
                _ => {}
            }
            HotResult::Ok(Val::from("unknown time"))
        }
        Val::Int(timestamp) => HotResult::Ok(Val::from(format!("{}", timestamp))),
        _ => HotResult::Ok(Val::from(format!("{:?}", args[0]))),
    }
}

/// Get year from a time value
pub fn year(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/year", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(m.get(&Val::from("year")).cloned().unwrap_or(Val::Null)),
        _ => HotResult::Err(Val::from("year: expected a temporal value")),
    }
}

pub fn month(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/month", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(m.get(&Val::from("month")).cloned().unwrap_or(Val::Null)),
        _ => HotResult::Err(Val::from("month: expected a temporal value")),
    }
}

pub fn day(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/day", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(m.get(&Val::from("day")).cloned().unwrap_or(Val::Null)),
        _ => HotResult::Err(Val::from("day: expected a temporal value")),
    }
}

pub fn hour(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/hour", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(m.get(&Val::from("hour")).cloned().unwrap_or(Val::Null)),
        _ => HotResult::Err(Val::from("hour: expected a temporal value")),
    }
}

pub fn minute(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/minute", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(m.get(&Val::from("minute")).cloned().unwrap_or(Val::Null)),
        _ => HotResult::Err(Val::from("minute: expected a temporal value")),
    }
}

pub fn second(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/second", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(m.get(&Val::from("second")).cloned().unwrap_or(Val::Null)),
        _ => HotResult::Err(Val::from("second: expected a temporal value")),
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers: Hot Val maps <-> temporal_rs types
// ---------------------------------------------------------------------------

/// Extract an integer field from a map, defaulting to 0.
fn get_int_field(m: &IndexMap<Val, Val>, key: &str) -> i64 {
    match m.get(&Val::from(key)) {
        Some(Val::Int(i)) => *i,
        Some(Val::Dec(d)) => d.to_f64() as i64,
        _ => 0,
    }
}

/// Convert a Hot Val temporal map to a temporal_rs::PlainDateTime.
/// Works for both PlainDateTime and PlainDate maps (PlainDate gets midnight time).
fn val_to_plain_datetime(val: &Val) -> Result<temporal_rs::PlainDateTime, String> {
    let m = match val {
        Val::Map(m) => m,
        _ => return Err("expected a temporal value (PlainDateTime or PlainDate)".to_string()),
    };

    let year = get_int_field(m, "year") as i32;
    let month = get_int_field(m, "month") as u8;
    let day = get_int_field(m, "day") as u8;
    let hour = get_int_field(m, "hour") as u8;
    let minute = get_int_field(m, "minute") as u8;
    let second = get_int_field(m, "second") as u8;
    let millisecond = get_int_field(m, "millisecond") as u16;
    let microsecond = get_int_field(m, "microsecond") as u16;
    let nanosecond = get_int_field(m, "nanosecond") as u16;

    temporal_rs::PlainDateTime::try_new_iso(
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
        microsecond,
        nanosecond,
    )
    .map_err(|e| format!("invalid date/time: {}", e))
}

/// Convert a Hot Duration map to a temporal_rs::Duration.
fn val_to_duration(val: &Val) -> Result<TemporalDuration, String> {
    let m = match val {
        Val::Map(m) => m,
        _ => return Err("expected a Duration value".to_string()),
    };

    TemporalDuration::new(
        get_int_field(m, "years"),
        get_int_field(m, "months"),
        get_int_field(m, "weeks"),
        get_int_field(m, "days"),
        get_int_field(m, "hours"),
        get_int_field(m, "minutes"),
        get_int_field(m, "seconds"),
        get_int_field(m, "milliseconds"),
        get_int_field(m, "microseconds") as i128,
        get_int_field(m, "nanoseconds") as i128,
    )
    .map_err(|e| format!("invalid duration: {}", e))
}

/// Convert a temporal_rs::Duration to a Hot Duration map.
fn duration_to_val(d: &TemporalDuration) -> Val {
    let mut m = IndexMap::new();
    m.insert(Val::from("$type"), Val::from("::hot::time/Duration"));
    let to_dec_i64 = |v: i64| Val::Dec((v as f64).into());
    let to_dec_i128 = |v: i128| Val::Dec((v as f64).into());
    m.insert(Val::from("years"), to_dec_i64(d.years()));
    m.insert(Val::from("months"), to_dec_i64(d.months()));
    m.insert(Val::from("weeks"), to_dec_i64(d.weeks()));
    m.insert(Val::from("days"), to_dec_i64(d.days()));
    m.insert(Val::from("hours"), to_dec_i64(d.hours()));
    m.insert(Val::from("minutes"), to_dec_i64(d.minutes()));
    m.insert(Val::from("seconds"), Val::Dec((d.seconds() as f64).into()));
    m.insert(Val::from("milliseconds"), to_dec_i64(d.milliseconds()));
    m.insert(Val::from("microseconds"), to_dec_i128(d.microseconds()));
    m.insert(Val::from("nanoseconds"), to_dec_i128(d.nanoseconds()));
    Val::Map(Box::new(m))
}

/// Convert a temporal_rs::PlainDateTime back into a Hot PlainDateTime map.
fn plain_datetime_to_val(dt: &temporal_rs::PlainDateTime) -> Val {
    let mut data = IndexMap::new();
    data.insert(Val::from("year"), Val::Int(dt.year() as i64));
    data.insert(Val::from("month"), Val::Int(dt.month() as i64));
    data.insert(Val::from("day"), Val::Int(dt.day() as i64));
    data.insert(Val::from("hour"), Val::Int(dt.hour() as i64));
    data.insert(Val::from("minute"), Val::Int(dt.minute() as i64));
    data.insert(Val::from("second"), Val::Int(dt.second() as i64));
    data.insert(Val::from("millisecond"), Val::Int(dt.millisecond() as i64));
    data.insert(Val::from("microsecond"), Val::Int(dt.microsecond() as i64));
    data.insert(Val::from("nanosecond"), Val::Int(dt.nanosecond() as i64));
    data.insert(
        Val::from("calendar"),
        Val::from(dt.calendar().identifier().to_string()),
    );
    create_temporal_map("PlainDateTime", data)
}

// ---------------------------------------------------------------------------
// Arithmetic: add, subtract, until, since
// ---------------------------------------------------------------------------

/// Helper: convert Result<T, String> to HotResult via a formatting prefix.
fn result_to_hot<T>(r: Result<T, String>, prefix: &str) -> Result<T, Val> {
    r.map_err(|e| Val::from(format!("{}: {}", prefix, e)))
}

/// Add a Duration to a PlainDateTime (or PlainDate promoted to midnight).
pub fn add(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/add", args, 2);
    let prefix = "::hot::time/add";

    let dt = match result_to_hot(val_to_plain_datetime(&args[0]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };
    let dur = match result_to_hot(val_to_duration(&args[1]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match dt.add(&dur, None) {
        Ok(result) => HotResult::Ok(plain_datetime_to_val(&result)),
        Err(e) => HotResult::Err(Val::from(format!("{}: {}", prefix, e))),
    }
}

/// Subtract a Duration from a PlainDateTime (or PlainDate promoted to midnight).
pub fn subtract(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/subtract", args, 2);
    let prefix = "::hot::time/subtract";

    let dt = match result_to_hot(val_to_plain_datetime(&args[0]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };
    let dur = match result_to_hot(val_to_duration(&args[1]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match dt.subtract(&dur, None) {
        Ok(result) => HotResult::Ok(plain_datetime_to_val(&result)),
        Err(e) => HotResult::Err(Val::from(format!("{}: {}", prefix, e))),
    }
}

/// Calculate the Duration from `start` to `end`.
pub fn until(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/until", args, 2);
    let prefix = "::hot::time/until";

    let start = match result_to_hot(val_to_plain_datetime(&args[0]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };
    let end = match result_to_hot(val_to_plain_datetime(&args[1]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match start.until(&end, Default::default()) {
        Ok(dur) => HotResult::Ok(duration_to_val(&dur)),
        Err(e) => HotResult::Err(Val::from(format!("{}: {}", prefix, e))),
    }
}

/// Calculate the Duration from `end` back to `start` (reverse of until).
pub fn since(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/since", args, 2);
    let prefix = "::hot::time/since";

    let start = match result_to_hot(val_to_plain_datetime(&args[0]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };
    let end = match result_to_hot(val_to_plain_datetime(&args[1]), prefix) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };

    match start.since(&end, Default::default()) {
        Ok(dur) => HotResult::Ok(duration_to_val(&dur)),
        Err(e) => HotResult::Err(Val::from(format!("{}: {}", prefix, e))),
    }
}

// Type constructors for time units
// Each constructor takes a value and returns {"$type": "::hot::time/TypeName", "$val": value}

/// Create a Millisecond typed object
pub fn millisecond_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Millisecond", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Millisecond"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Second typed object
pub fn second_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Second", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Second"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Minute typed object
pub fn minute_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Minute", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Minute"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create an Hour typed object
pub fn hour_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Hour", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Hour"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Day typed object
pub fn day_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Day", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Day"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Week typed object
pub fn week_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Week", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Week"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Month typed object
pub fn month_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Month", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Month"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Year typed object
pub fn year_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Year", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Year"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Nanosecond typed object
pub fn nanosecond_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Nanosecond", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Nanosecond"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Create a Microsecond typed object
pub fn microsecond_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/Microsecond", args, 1);

    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::time/Microsecond"));
    result.insert(Val::from("$val"), args[0].clone());
    HotResult::Ok(Val::Map(Box::new(result)))
}

// Removed instant_constructor - Hot type-based overloads now handle dispatch correctly

/// Create an Instant from milliseconds
pub fn instant_from_millis(args: &[Val]) -> HotResult<Val> {
    tracing::debug!(
        "time::instant_from_millis called with {} args: {:?}",
        args.len(),
        args
    );
    validate_args!("instant-from-millis", args, 1);

    // Extract milliseconds from Millisecond type
    let millis = match &args[0] {
        Val::Map(millisecond_map) => {
            if let Some(Val::Int(millis)) = millisecond_map.get(&Val::from("$val")) {
                *millis
            } else {
                return HotResult::Err(Val::from(
                    "Invalid Millisecond object: missing $val".to_string(),
                ));
            }
        }
        Val::Int(millis) => *millis,
        _ => {
            return HotResult::Err(Val::from(
                "instant-from-millis expects a Millisecond or Int".to_string(),
            ));
        }
    };

    // Convert milliseconds to nanoseconds (with overflow protection)
    let nanos = millis.saturating_mul(1_000_000);

    // Create Instant object with epochNanoseconds
    let mut data = IndexMap::new();
    data.insert(Val::from("epochNanoseconds"), Val::Int(nanos));
    HotResult::Ok(create_temporal_map("Instant", data))
}

/// Parse a string into a PlainDate typed value
pub fn parse_plain_date(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(
            "::hot::time/parse-plain-date expects 1 argument".to_string(),
        ));
    }
    match &args[0] {
        Val::Str(_) => parse(args),
        other => HotResult::Err(Val::from(format!(
            "parse-plain-date expects Str, got {:?}",
            other
        ))),
    }
}

/// Parse a string into a PlainTime typed value
pub fn parse_plain_time(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(
            "::hot::time/parse-plain-time expects 1 argument".to_string(),
        ));
    }
    match &args[0] {
        Val::Str(_) => parse(args),
        other => HotResult::Err(Val::from(format!(
            "parse-plain-time expects Str, got {:?}",
            other
        ))),
    }
}

/// Parse a string into a PlainDateTime typed value
pub fn parse_plain_date_time(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(
            "::hot::time/parse-plain-date-time expects 1 argument".to_string(),
        ));
    }
    match &args[0] {
        Val::Str(_) => parse(args),
        other => HotResult::Err(Val::from(format!(
            "parse-plain-date-time expects Str, got {:?}",
            other
        ))),
    }
}

/// Current local date as PlainDate
pub fn now_plain_date(_args: &[Val]) -> HotResult<Val> {
    let today = chrono::Local::now().date_naive();
    let mut m = IndexMap::new();
    m.insert(Val::from("$type"), Val::from("::hot::time/PlainDate"));
    m.insert(Val::from("year"), Val::Int(today.year() as i64));
    m.insert(Val::from("month"), Val::Int(today.month() as i64));
    m.insert(Val::from("day"), Val::Int(today.day() as i64));
    HotResult::Ok(Val::Map(Box::new(m)))
}

/// Current local time as PlainTime (hour, minute, second)
pub fn now_plain_time(_args: &[Val]) -> HotResult<Val> {
    let now = chrono::Local::now().time();
    let mut m = IndexMap::new();
    m.insert(Val::from("$type"), Val::from("::hot::time/PlainTime"));
    m.insert(Val::from("hour"), Val::Int(now.hour() as i64));
    m.insert(Val::from("minute"), Val::Int(now.minute() as i64));
    m.insert(Val::from("second"), Val::Int(now.second() as i64));
    HotResult::Ok(Val::Map(Box::new(m)))
}

/// Current local date-time as PlainDateTime
pub fn now_plain_date_time(_args: &[Val]) -> HotResult<Val> {
    let now = chrono::Local::now().naive_local();
    let mut m = IndexMap::new();
    m.insert(Val::from("$type"), Val::from("::hot::time/PlainDateTime"));
    m.insert(Val::from("year"), Val::Int(now.year() as i64));
    m.insert(Val::from("month"), Val::Int(now.month() as i64));
    m.insert(Val::from("day"), Val::Int(now.day() as i64));
    m.insert(Val::from("hour"), Val::Int(now.hour() as i64));
    m.insert(Val::from("minute"), Val::Int(now.minute() as i64));
    m.insert(Val::from("second"), Val::Int(now.second() as i64));
    HotResult::Ok(Val::Map(Box::new(m)))
}
/// Create a PlainDate(year, month, day)
pub fn plain_date_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/PlainDate", args, 3);
    let year = match &args[0] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDate year must be Int")),
    };
    let month = match &args[1] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDate month must be Int")),
    };
    let day = match &args[2] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDate day must be Int")),
    };
    let mut m = IndexMap::new();
    m.insert(Val::from("$type"), Val::from("::hot::time/PlainDate"));
    m.insert(Val::from("year"), Val::Int(year));
    m.insert(Val::from("month"), Val::Int(month));
    m.insert(Val::from("day"), Val::Int(day));
    HotResult::Ok(Val::Map(Box::new(m)))
}

/// Create a PlainTime(hour, minute, second)
pub fn plain_time_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/PlainTime", args, 3);
    let hour = match &args[0] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainTime hour must be Int")),
    };
    let minute = match &args[1] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainTime minute must be Int")),
    };
    let second = match &args[2] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainTime second must be Int")),
    };
    let mut m = IndexMap::new();
    m.insert(Val::from("$type"), Val::from("::hot::time/PlainTime"));
    m.insert(Val::from("hour"), Val::Int(hour));
    m.insert(Val::from("minute"), Val::Int(minute));
    m.insert(Val::from("second"), Val::Int(second));
    HotResult::Ok(Val::Map(Box::new(m)))
}

/// Create a PlainDateTime(year, month, day, hour, minute, second)
pub fn plain_datetime_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/PlainDateTime", args, 6);
    let year = match &args[0] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDateTime year must be Int")),
    };
    let month = match &args[1] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDateTime month must be Int")),
    };
    let day = match &args[2] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDateTime day must be Int")),
    };
    let hour = match &args[3] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDateTime hour must be Int")),
    };
    let minute = match &args[4] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDateTime minute must be Int")),
    };
    let second = match &args[5] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("PlainDateTime second must be Int")),
    };
    let mut m = IndexMap::new();
    m.insert(Val::from("$type"), Val::from("::hot::time/PlainDateTime"));
    m.insert(Val::from("year"), Val::Int(year));
    m.insert(Val::from("month"), Val::Int(month));
    m.insert(Val::from("day"), Val::Int(day));
    m.insert(Val::from("hour"), Val::Int(hour));
    m.insert(Val::from("minute"), Val::Int(minute));
    m.insert(Val::from("second"), Val::Int(second));
    HotResult::Ok(Val::Map(Box::new(m)))
}

/// Create a Duration(value: map or string)
pub fn duration_constructor(args: &[Val]) -> HotResult<Val> {
    tracing::debug!(
        "duration_constructor called with {} args: {:?}",
        args.len(),
        args
    );
    validate_args!("::hot::time/Duration", args, 1);
    // Accept either a map of components or an ISO 8601 string
    match &args[0] {
        Val::Map(input) => {
            let mut m = IndexMap::new();
            m.insert(Val::from("$type"), Val::from("::hot::time/Duration"));
            // Normalize known fields to Dec
            let norm = |key: &str| -> Val {
                match input.get(&Val::from(key.to_string())) {
                    Some(Val::Dec(d)) => Val::Dec(*d),
                    Some(Val::Int(i)) => Val::Dec((*i as f64).into()),
                    Some(other) => other.clone(),
                    None => Val::Dec(0.0.into()),
                }
            };
            for k in [
                "years",
                "months",
                "weeks",
                "days",
                "hours",
                "minutes",
                "seconds",
                "milliseconds",
                "microseconds",
                "nanoseconds",
            ] {
                m.insert(Val::from(k.to_string()), norm(k));
            }
            HotResult::Ok(Val::Map(Box::new(m)))
        }
        Val::Str(s) => {
            // Parse ISO 8601 duration string using temporal_rs
            match TemporalDuration::from_str(s) {
                Ok(d) => {
                    let mut m = IndexMap::new();
                    m.insert(Val::from("$type"), Val::from("::hot::time/Duration"));
                    let to_dec_i64 = |v: i64| Val::Dec((v as f64).into());
                    let to_dec_i128 = |v: i128| Val::Dec((v as f64).into());
                    m.insert(Val::from("years"), to_dec_i64(d.years()));
                    m.insert(Val::from("months"), to_dec_i64(d.months()));
                    m.insert(Val::from("weeks"), to_dec_i64(d.weeks()));
                    m.insert(Val::from("days"), to_dec_i64(d.days()));
                    m.insert(Val::from("hours"), to_dec_i64(d.hours()));
                    m.insert(Val::from("minutes"), to_dec_i64(d.minutes()));
                    m.insert(Val::from("seconds"), Val::Dec((d.seconds() as f64).into()));
                    m.insert(Val::from("milliseconds"), to_dec_i64(d.milliseconds()));
                    m.insert(Val::from("microseconds"), to_dec_i128(d.microseconds()));
                    m.insert(Val::from("nanoseconds"), to_dec_i128(d.nanoseconds()));
                    HotResult::Ok(Val::Map(Box::new(m)))
                }
                Err(_) => HotResult::Err(Val::from("Invalid ISO 8601 duration string")),
            }
        }
        _ => HotResult::Err(Val::from(
            "::hot::time/Duration expects a map or string".to_string(),
        )),
    }
}

/// Get millisecond from a time value (default 0 if absent)
pub fn millisecond(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/millisecond", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(
            m.get(&Val::from("millisecond"))
                .cloned()
                .unwrap_or(Val::Int(0)),
        ),
        _ => HotResult::Ok(Val::Int(0)),
    }
}

/// Get microsecond from a time value (default 0 if absent)
pub fn microsecond(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/microsecond", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(
            m.get(&Val::from("microsecond"))
                .cloned()
                .unwrap_or(Val::Int(0)),
        ),
        _ => HotResult::Ok(Val::Int(0)),
    }
}

/// Get nanosecond from a time value (default 0 if absent)
pub fn nanosecond(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/nanosecond", args, 1);
    match &args[0] {
        Val::Map(m) => HotResult::Ok(
            m.get(&Val::from("nanosecond"))
                .cloned()
                .unwrap_or(Val::Int(0)),
        ),
        _ => HotResult::Ok(Val::Int(0)),
    }
}

/// Parse a time string (very minimal; return fixed date for tests)
pub fn parse(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/parse", args, 1);

    let input = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("parse requires a string input")),
    };

    // Try to parse as different temporal types.
    if let Ok(datetime) = temporal_rs::PlainDateTime::from_str(input) {
        let mut data = indexmap::IndexMap::new();
        data.insert(Val::from("year"), Val::Int(datetime.year() as i64));
        data.insert(Val::from("month"), Val::Int(datetime.month() as i64));
        data.insert(Val::from("day"), Val::Int(datetime.day() as i64));
        data.insert(Val::from("hour"), Val::Int(datetime.hour() as i64));
        data.insert(Val::from("minute"), Val::Int(datetime.minute() as i64));
        data.insert(Val::from("second"), Val::Int(datetime.second() as i64));
        data.insert(
            Val::from("millisecond"),
            Val::Int(datetime.millisecond() as i64),
        );
        data.insert(
            Val::from("microsecond"),
            Val::Int(datetime.microsecond() as i64),
        );
        data.insert(
            Val::from("nanosecond"),
            Val::Int(datetime.nanosecond() as i64),
        );
        data.insert(
            Val::from("calendar"),
            Val::from(datetime.calendar().identifier().to_string()),
        );
        return HotResult::Ok(create_temporal_map("PlainDateTime", data));
    }

    if let Ok(time) = temporal_rs::PlainTime::from_str(input) {
        let mut data = indexmap::IndexMap::new();
        data.insert(Val::from("hour"), Val::Int(time.hour() as i64));
        data.insert(Val::from("minute"), Val::Int(time.minute() as i64));
        data.insert(Val::from("second"), Val::Int(time.second() as i64));
        data.insert(
            Val::from("millisecond"),
            Val::Int(time.millisecond() as i64),
        );
        data.insert(
            Val::from("microsecond"),
            Val::Int(time.microsecond() as i64),
        );
        data.insert(Val::from("nanosecond"), Val::Int(time.nanosecond() as i64));
        return HotResult::Ok(create_temporal_map("PlainTime", data));
    }

    if let Ok(date) = temporal_rs::PlainDate::from_str(input) {
        let mut data = indexmap::IndexMap::new();
        data.insert(Val::from("year"), Val::Int(date.year() as i64));
        data.insert(Val::from("month"), Val::Int(date.month() as i64));
        data.insert(Val::from("day"), Val::Int(date.day() as i64));
        data.insert(
            Val::from("calendar"),
            Val::from(date.calendar().identifier().to_string()),
        );
        return HotResult::Ok(create_temporal_map("PlainDate", data));
    }

    HotResult::Err(Val::from(
        "Unable to parse string as temporal object".to_string(),
    ))
}

/// Format a time value (delegate to to_string)
pub fn format(args: &[Val]) -> HotResult<Val> {
    to_string(args)
}

/// Create an instant from epoch microseconds
pub fn instant_from_micros(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/instant-from-micros", args, 1);

    // Strip type wrapping: Microsecond{$val: 1000} → 1000
    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    let micros = match &untyped {
        Val::Int(i) => *i,
        Val::Dec(d) => d.floor().to_i64().unwrap_or(0),
        _ => {
            return HotResult::Err(Val::from(
                "instant-from-micros requires Int, Dec, or Microsecond".to_string(),
            ));
        }
    };

    // Convert microseconds to nanoseconds
    let nanos = micros.saturating_mul(1_000);

    let mut data = IndexMap::new();
    data.insert(Val::from("epochNanoseconds"), Val::Int(nanos));
    HotResult::Ok(create_temporal_map("Instant", data))
}

/// Create an instant from epoch nanoseconds
pub fn instant_from_nanos(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/instant-from-nanos", args, 1);

    // Strip type wrapping: Nanosecond{$val: 1000} → 1000
    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    let nanos = match &untyped {
        Val::Int(i) => *i,
        Val::Dec(d) => d.floor().to_i64().unwrap_or(0),
        _ => {
            return HotResult::Err(Val::from(
                "instant-from-nanos requires Int, Dec, or Nanosecond".to_string(),
            ));
        }
    };

    let mut data = IndexMap::new();
    data.insert(Val::from("epochNanoseconds"), Val::Int(nanos));
    HotResult::Ok(create_temporal_map("Instant", data))
}

/// Create an instant from a PlainDate (assumes start of day UTC)
pub fn instant_from_plain_date(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/instant-from-plain-date", args, 1);

    // Strip type wrapping so typed PlainDate and structural Maps both work
    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    let data = match &untyped {
        Val::Map(map) => map,
        _ => {
            return HotResult::Err(Val::from(
                "Expected PlainDate object or Map with {year, month, day}",
            ));
        }
    };

    let year = match data.get(&Val::from("year")) {
        Some(Val::Int(y)) => *y as i32,
        _ => return HotResult::Err(Val::from("Missing or invalid year")),
    };

    let month = match data.get(&Val::from("month")) {
        Some(Val::Int(m)) => *m as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid month")),
    };

    let day = match data.get(&Val::from("day")) {
        Some(Val::Int(d)) => *d as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid day")),
    };

    // Create a naive datetime at start of day (00:00:00)
    match chrono::NaiveDate::from_ymd_opt(year, month, day) {
        Some(date) => {
            let datetime = date.and_hms_opt(0, 0, 0).unwrap();
            let timestamp = datetime.and_utc().timestamp_nanos_opt().unwrap_or(0);

            let mut instant_map = IndexMap::new();
            instant_map.insert(Val::from("epochNanoseconds"), Val::Int(timestamp));
            HotResult::Ok(Val::Map(Box::new(instant_map)))
        }
        None => HotResult::Err(Val::from("Invalid date values")),
    }
}

/// Create an instant from a PlainTime (assumes today's date UTC)
pub fn instant_from_plain_time(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/instant-from-plain-time", args, 1);

    // Strip type wrapping so typed PlainTime and structural Maps both work
    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    let data = match &untyped {
        Val::Map(map) => map,
        _ => {
            return HotResult::Err(Val::from(
                "Expected PlainTime object or Map with {hour, minute, second}",
            ));
        }
    };

    let hour = match data.get(&Val::from("hour")) {
        Some(Val::Int(h)) => *h as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid hour")),
    };

    let minute = match data.get(&Val::from("minute")) {
        Some(Val::Int(m)) => *m as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid minute")),
    };

    let second = match data.get(&Val::from("second")) {
        Some(Val::Int(s)) => *s as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid second")),
    };

    let millisecond = match data.get(&Val::from("millisecond")) {
        Some(Val::Int(ms)) => *ms as u32,
        _ => 0,
    };

    let microsecond = match data.get(&Val::from("microsecond")) {
        Some(Val::Int(us)) => *us as u32,
        _ => 0,
    };

    let nanosecond = match data.get(&Val::from("nanosecond")) {
        Some(Val::Int(ns)) => *ns as u32,
        _ => 0,
    };

    // Get today's date in UTC
    let today = chrono::Utc::now().date_naive();

    // Combine nanosecond components
    let total_nanos = nanosecond
        .saturating_add(microsecond.saturating_mul(1_000))
        .saturating_add(millisecond.saturating_mul(1_000_000));

    match today.and_hms_nano_opt(hour, minute, second, total_nanos) {
        Some(datetime) => {
            let timestamp = datetime.and_utc().timestamp_nanos_opt().unwrap_or(0);

            let mut instant_map = IndexMap::new();
            instant_map.insert(Val::from("epochNanoseconds"), Val::Int(timestamp));
            HotResult::Ok(Val::Map(Box::new(instant_map)))
        }
        None => HotResult::Err(Val::from("Invalid time values")),
    }
}

/// Create an instant from a PlainDateTime (assumes UTC)
pub fn instant_from_plain_date_time(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/instant-from-plain-date-time", args, 1);

    // Strip type wrapping so typed PlainDateTime and structural Maps both work
    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    let data = match &untyped {
        Val::Map(map) => map,
        _ => {
            return HotResult::Err(Val::from(
                "Expected PlainDateTime object or Map with {year, month, day, hour, minute, second}",
            ));
        }
    };

    let year = match data.get(&Val::from("year")) {
        Some(Val::Int(y)) => *y as i32,
        _ => return HotResult::Err(Val::from("Missing or invalid year")),
    };

    let month = match data.get(&Val::from("month")) {
        Some(Val::Int(m)) => *m as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid month")),
    };

    let day = match data.get(&Val::from("day")) {
        Some(Val::Int(d)) => *d as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid day")),
    };

    let hour = match data.get(&Val::from("hour")) {
        Some(Val::Int(h)) => *h as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid hour")),
    };

    let minute = match data.get(&Val::from("minute")) {
        Some(Val::Int(m)) => *m as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid minute")),
    };

    let second = match data.get(&Val::from("second")) {
        Some(Val::Int(s)) => *s as u32,
        _ => return HotResult::Err(Val::from("Missing or invalid second")),
    };

    let millisecond = match data.get(&Val::from("millisecond")) {
        Some(Val::Int(ms)) => *ms as u32,
        _ => 0,
    };

    let microsecond = match data.get(&Val::from("microsecond")) {
        Some(Val::Int(us)) => *us as u32,
        _ => 0,
    };

    let nanosecond = match data.get(&Val::from("nanosecond")) {
        Some(Val::Int(ns)) => *ns as u32,
        _ => 0,
    };

    // Combine nanosecond components
    let total_nanos = nanosecond
        .saturating_add(microsecond.saturating_mul(1_000))
        .saturating_add(millisecond.saturating_mul(1_000_000));

    match chrono::NaiveDate::from_ymd_opt(year, month, day) {
        Some(date) => match date.and_hms_nano_opt(hour, minute, second, total_nanos) {
            Some(datetime) => {
                let timestamp = datetime.and_utc().timestamp_nanos_opt().unwrap_or(0);

                let mut instant_map = IndexMap::new();
                instant_map.insert(Val::from("$type"), Val::from("::hot::time/Instant"));
                instant_map.insert(Val::from("epochNanoseconds"), Val::Int(timestamp));
                HotResult::Ok(Val::Map(Box::new(instant_map)))
            }
            None => HotResult::Err(Val::from("Invalid time values")),
        },
        None => HotResult::Err(Val::from("Invalid date values")),
    }
}

/// Parse an ISO 8601 duration string into a Duration object
pub fn parse_duration(args: &[Val]) -> HotResult<Val> {
    tracing::debug!("parse_duration called with {} args: {:?}", args.len(), args);
    validate_args!("::hot::time/parse-duration", args, 1);

    let input = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "parse-duration requires a string input [CALLED FROM parse_duration FUNCTION]"
                    .to_string(),
            ));
        }
    };

    match TemporalDuration::from_str(input) {
        Ok(d) => {
            let mut data = IndexMap::new();
            data.insert(Val::from("$type"), Val::from("::hot::time/Duration"));
            // Use Dec for all fields to match Duration type definition in Hot std
            let to_dec_i64 = |v: i64| Val::Dec((v as f64).into());
            let to_dec_i128 = |v: i128| Val::Dec((v as f64).into());
            data.insert(Val::from("years"), to_dec_i64(d.years()));
            data.insert(Val::from("months"), to_dec_i64(d.months()));
            data.insert(Val::from("weeks"), to_dec_i64(d.weeks()));
            data.insert(Val::from("days"), to_dec_i64(d.days()));
            data.insert(Val::from("hours"), to_dec_i64(d.hours()));
            data.insert(Val::from("minutes"), to_dec_i64(d.minutes()));
            data.insert(Val::from("seconds"), Val::Dec((d.seconds() as f64).into()));
            data.insert(Val::from("milliseconds"), to_dec_i64(d.milliseconds()));
            data.insert(Val::from("microseconds"), to_dec_i128(d.microseconds()));
            data.insert(Val::from("nanoseconds"), to_dec_i128(d.nanoseconds()));
            HotResult::Ok(Val::Map(Box::new(data)))
        }
        Err(_) => HotResult::Err(Val::from("Unable to parse ISO 8601 duration")),
    }
}

/// Create a simple duration object (without full temporal_rs complexity for now)
pub fn duration(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(
            "duration requires a single map argument".to_string(),
        ));
    }

    if let Val::Map(map) = &args[0] {
        let mut data = IndexMap::new();
        data.insert(Val::from("$type"), Val::from("::hot::time/Duration"));

        // Extract values with defaults of 0
        let years = map
            .get(&Val::from("years"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as f64),
                Val::Dec(d) => Some(d.to_f64()),
                _ => None,
            })
            .unwrap_or(0.0);

        let months = map
            .get(&Val::from("months"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as f64),
                Val::Dec(d) => Some(d.to_f64()),
                _ => None,
            })
            .unwrap_or(0.0);

        let days = map
            .get(&Val::from("days"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as f64),
                Val::Dec(d) => Some(d.to_f64()),
                _ => None,
            })
            .unwrap_or(0.0);

        let hours = map
            .get(&Val::from("hours"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as f64),
                Val::Dec(d) => Some(d.to_f64()),
                _ => None,
            })
            .unwrap_or(0.0);

        let minutes = map
            .get(&Val::from("minutes"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as f64),
                Val::Dec(d) => Some(d.to_f64()),
                _ => None,
            })
            .unwrap_or(0.0);

        let seconds = map
            .get(&Val::from("seconds"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as f64),
                Val::Dec(d) => Some(d.to_f64()),
                _ => None,
            })
            .unwrap_or(0.0);

        data.insert(Val::from("years"), Val::Dec(years.into()));
        data.insert(Val::from("months"), Val::Dec(months.into()));
        data.insert(Val::from("days"), Val::Dec(days.into()));
        data.insert(Val::from("hours"), Val::Dec(hours.into()));
        data.insert(Val::from("minutes"), Val::Dec(minutes.into()));
        data.insert(Val::from("seconds"), Val::Dec(seconds.into()));

        HotResult::Ok(Val::Map(Box::new(data)))
    } else {
        HotResult::Err(Val::from(
            "duration requires a map of duration fields".to_string(),
        ))
    }
}

/// Create a PlainDate from year, month, day
pub fn plain_date(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/plain-date", args, 3);

    let year = match &args[0] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("year must be an integer")),
    };

    let month = match &args[1] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("month must be an integer")),
    };

    let day = match &args[2] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("day must be an integer")),
    };

    let mut data = IndexMap::new();
    data.insert(Val::from("$type"), Val::from("::hot::time/PlainDate"));
    data.insert(Val::from("year"), Val::Int(year));
    data.insert(Val::from("month"), Val::Int(month));
    data.insert(Val::from("day"), Val::Int(day));
    data.insert(Val::from("calendar"), Val::from("iso8601"));
    HotResult::Ok(Val::Map(Box::new(data)))
}

/// Create a PlainTime from hour, minute, second
pub fn plain_time(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() {
        return HotResult::Err(Val::from("plain-time requires at least hour"));
    }

    let hour = match &args[0] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("hour must be an integer")),
    };

    let minute = match args.get(1) {
        Some(Val::Int(i)) => *i,
        Some(_) => return HotResult::Err(Val::from("minute must be an integer")),
        None => 0,
    };

    let second = match args.get(2) {
        Some(Val::Int(i)) => *i,
        Some(_) => return HotResult::Err(Val::from("second must be an integer")),
        None => 0,
    };

    let mut data = IndexMap::new();
    data.insert(Val::from("$type"), Val::from("::hot::time/PlainTime"));
    data.insert(Val::from("hour"), Val::Int(hour));
    data.insert(Val::from("minute"), Val::Int(minute));
    data.insert(Val::from("second"), Val::Int(second));
    data.insert(Val::from("millisecond"), Val::Int(0));
    data.insert(Val::from("microsecond"), Val::Int(0));
    data.insert(Val::from("nanosecond"), Val::Int(0));
    HotResult::Ok(Val::Map(Box::new(data)))
}

/// Create a PlainDateTime from year, month, day, hour, minute, second
pub fn plain_date_time(args: &[Val]) -> HotResult<Val> {
    if args.len() < 3 {
        return HotResult::Err(Val::from(
            "plain-date-time requires at least year, month, day".to_string(),
        ));
    }

    let year = match &args[0] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("year must be an integer")),
    };

    let month = match &args[1] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("month must be an integer")),
    };

    let day = match &args[2] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("day must be an integer")),
    };

    let hour = match args.get(3) {
        Some(Val::Int(i)) => *i,
        Some(_) => return HotResult::Err(Val::from("hour must be an integer")),
        None => 0,
    };

    let minute = match args.get(4) {
        Some(Val::Int(i)) => *i,
        Some(_) => return HotResult::Err(Val::from("minute must be an integer")),
        None => 0,
    };

    let second = match args.get(5) {
        Some(Val::Int(i)) => *i,
        Some(_) => return HotResult::Err(Val::from("second must be an integer")),
        None => 0,
    };

    let mut data = IndexMap::new();
    data.insert(Val::from("$type"), Val::from("::hot::time/PlainDateTime"));
    data.insert(Val::from("year"), Val::Int(year));
    data.insert(Val::from("month"), Val::Int(month));
    data.insert(Val::from("day"), Val::Int(day));
    data.insert(Val::from("hour"), Val::Int(hour));
    data.insert(Val::from("minute"), Val::Int(minute));
    data.insert(Val::from("second"), Val::Int(second));
    data.insert(Val::from("millisecond"), Val::Int(0));
    data.insert(Val::from("microsecond"), Val::Int(0));
    data.insert(Val::from("nanosecond"), Val::Int(0));
    data.insert(Val::from("calendar"), Val::from("iso8601"));

    HotResult::Ok(Val::Map(Box::new(data)))
}

// ---------------------------------------------------------------------------
// ZonedDateTime
// ---------------------------------------------------------------------------

/// Convert a temporal_rs::ZonedDateTime to a Hot ZonedDateTime Val map.
fn zoned_datetime_to_val(zdt: &TemporalZonedDateTime) -> Val {
    let mut data = IndexMap::new();
    data.insert(Val::from("$type"), Val::from("::hot::time/ZonedDateTime"));
    data.insert(
        Val::from("epochNanoseconds"),
        Val::Int(zdt.epoch_nanoseconds().as_i128() as i64),
    );
    let tz_id = zdt
        .time_zone()
        .identifier()
        .unwrap_or_else(|_| "UTC".to_string());
    data.insert(Val::from("timezone"), Val::from(tz_id));
    data.insert(Val::from("offset"), Val::from(zdt.offset()));
    data.insert(Val::from("year"), Val::Int(zdt.year() as i64));
    data.insert(Val::from("month"), Val::Int(zdt.month() as i64));
    data.insert(Val::from("day"), Val::Int(zdt.day() as i64));
    data.insert(Val::from("hour"), Val::Int(zdt.hour() as i64));
    data.insert(Val::from("minute"), Val::Int(zdt.minute() as i64));
    data.insert(Val::from("second"), Val::Int(zdt.second() as i64));
    data.insert(Val::from("millisecond"), Val::Int(zdt.millisecond() as i64));
    data.insert(Val::from("microsecond"), Val::Int(zdt.microsecond() as i64));
    data.insert(Val::from("nanosecond"), Val::Int(zdt.nanosecond() as i64));
    data.insert(
        Val::from("calendar"),
        Val::from(zdt.calendar().identifier().to_string()),
    );
    Val::Map(Box::new(data))
}

/// Parse an IXDTF string into a ZonedDateTime.
/// Example: "2026-02-17T10:30:00-06:00[America/Chicago]"
pub fn zoned_date_time_from_string(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/zoned-date-time-from-string", args, 1);

    let input = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("zoned-date-time-from-string expects a string")),
    };

    match TemporalZonedDateTime::from_utf8(
        input.as_bytes(),
        Disambiguation::Compatible,
        OffsetDisambiguation::Reject,
    ) {
        Ok(zdt) => HotResult::Ok(zoned_datetime_to_val(&zdt)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to parse ZonedDateTime: {}", e))),
    }
}

/// Create a ZonedDateTime from an Instant + timezone string.
pub fn zoned_date_time_from_instant(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/zoned-date-time-from-instant", args, 2);

    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    let epoch_nanos = match &untyped {
        Val::Map(m) => match m.get(&Val::from("epochNanoseconds")) {
            Some(Val::Int(ns)) => *ns as i128,
            _ => {
                return HotResult::Err(Val::from(
                    "First argument must be an Instant with epochNanoseconds",
                ));
            }
        },
        _ => return HotResult::Err(Val::from("First argument must be an Instant")),
    };

    let tz_str = match &args[1] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("Second argument must be a timezone string")),
    };

    let time_zone = match TemporalTimeZone::try_from_str(tz_str) {
        Ok(tz) => tz,
        Err(e) => {
            return HotResult::Err(Val::from(format!("Invalid timezone '{}': {}", tz_str, e)));
        }
    };

    match TemporalZonedDateTime::try_new(epoch_nanos, time_zone, TemporalCalendar::default()) {
        Ok(zdt) => HotResult::Ok(zoned_datetime_to_val(&zdt)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to create ZonedDateTime: {}", e))),
    }
}

/// Create a ZonedDateTime from a PlainDateTime + timezone string.
pub fn zoned_date_time_from_plain_date_time(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/zoned-date-time-from-plain-date-time", args, 2);

    let pdt = match val_to_plain_datetime(&args[0]) {
        Ok(v) => v,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "First argument must be a PlainDateTime: {}",
                e
            )));
        }
    };

    let tz_str = match &args[1] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("Second argument must be a timezone string")),
    };

    let time_zone = match TemporalTimeZone::try_from_str(tz_str) {
        Ok(tz) => tz,
        Err(e) => {
            return HotResult::Err(Val::from(format!("Invalid timezone '{}': {}", tz_str, e)));
        }
    };

    match pdt.to_zoned_date_time(time_zone, Disambiguation::Compatible) {
        Ok(zdt) => HotResult::Ok(zoned_datetime_to_val(&zdt)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to create ZonedDateTime: {}", e))),
    }
}

/// Get the current time as a ZonedDateTime in the given timezone.
pub fn now_zoned(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/now-zoned", args, 1);

    let tz_str = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("now-zoned expects a timezone string")),
    };

    let epoch_nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(_) => return HotResult::Err(Val::from("Failed to get current time")),
    };

    let time_zone = match TemporalTimeZone::try_from_str(tz_str) {
        Ok(tz) => tz,
        Err(e) => {
            return HotResult::Err(Val::from(format!("Invalid timezone '{}': {}", tz_str, e)));
        }
    };

    match TemporalZonedDateTime::try_new(epoch_nanos, time_zone, TemporalCalendar::default()) {
        Ok(zdt) => HotResult::Ok(zoned_datetime_to_val(&zdt)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to create ZonedDateTime: {}", e))),
    }
}

/// Convert a ZonedDateTime to a different timezone.
pub fn with_timezone(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/with-timezone", args, 2);

    let untyped = match untype_recursive(&args[0]) {
        HotResult::Ok(v) => v,
        _ => args[0].clone(),
    };
    let epoch_nanos = match &untyped {
        Val::Map(m) => match m.get(&Val::from("epochNanoseconds")) {
            Some(Val::Int(ns)) => *ns as i128,
            _ => return HotResult::Err(Val::from("First argument must be a ZonedDateTime")),
        },
        _ => return HotResult::Err(Val::from("First argument must be a ZonedDateTime")),
    };

    let old_tz_str = match &untyped {
        Val::Map(m) => match m.get(&Val::from("timezone")) {
            Some(Val::Str(s)) => s.to_string(),
            _ => "UTC".to_string(),
        },
        _ => "UTC".to_string(),
    };

    let old_tz = match TemporalTimeZone::try_from_str(&old_tz_str) {
        Ok(tz) => tz,
        Err(_) => TemporalTimeZone::utc(),
    };

    let new_tz_str = match &args[1] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("Second argument must be a timezone string")),
    };

    let new_tz = match TemporalTimeZone::try_from_str(new_tz_str) {
        Ok(tz) => tz,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "Invalid timezone '{}': {}",
                new_tz_str, e
            )));
        }
    };

    // Reconstruct the original ZonedDateTime, then convert
    match TemporalZonedDateTime::try_new(epoch_nanos, old_tz, TemporalCalendar::default()) {
        Ok(zdt) => match zdt.with_timezone(new_tz) {
            Ok(new_zdt) => HotResult::Ok(zoned_datetime_to_val(&new_zdt)),
            Err(e) => HotResult::Err(Val::from(format!("Failed to convert timezone: {}", e))),
        },
        Err(e) => HotResult::Err(Val::from(format!(
            "Failed to reconstruct ZonedDateTime: {}",
            e
        ))),
    }
}

/// Extract PlainDateTime from a ZonedDateTime.
pub fn to_plain_date_time_from_zdt(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/to-plain-date-time", args, 1);

    let m = match &args[0] {
        Val::Map(m) => m,
        _ => return HotResult::Err(Val::from("to-plain-date-time expects a ZonedDateTime")),
    };

    let mut data = IndexMap::new();
    data.insert(Val::from("$type"), Val::from("::hot::time/PlainDateTime"));
    for key in &[
        "year",
        "month",
        "day",
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
        "calendar",
    ] {
        if let Some(v) = m.get(&Val::from(*key)) {
            data.insert(Val::from(*key), v.clone());
        }
    }
    HotResult::Ok(Val::Map(Box::new(data)))
}

/// Extract PlainDate from a ZonedDateTime.
pub fn to_plain_date_from_zdt(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/to-plain-date", args, 1);

    let m = match &args[0] {
        Val::Map(m) => m,
        _ => return HotResult::Err(Val::from("to-plain-date expects a ZonedDateTime")),
    };

    let mut data = IndexMap::new();
    data.insert(Val::from("$type"), Val::from("::hot::time/PlainDate"));
    for key in &["year", "month", "day", "calendar"] {
        if let Some(v) = m.get(&Val::from(*key)) {
            data.insert(Val::from(*key), v.clone());
        }
    }
    HotResult::Ok(Val::Map(Box::new(data)))
}

/// Extract PlainTime from a ZonedDateTime.
pub fn to_plain_time_from_zdt(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/to-plain-time", args, 1);

    let m = match &args[0] {
        Val::Map(m) => m,
        _ => return HotResult::Err(Val::from("to-plain-time expects a ZonedDateTime")),
    };

    let mut data = IndexMap::new();
    data.insert(Val::from("$type"), Val::from("::hot::time/PlainTime"));
    for key in &[
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
    ] {
        if let Some(v) = m.get(&Val::from(*key)) {
            data.insert(Val::from(*key), v.clone());
        }
    }
    HotResult::Ok(Val::Map(Box::new(data)))
}

/// Extract Instant from a ZonedDateTime.
pub fn to_instant_from_zdt(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/to-instant", args, 1);

    let m = match &args[0] {
        Val::Map(m) => m,
        _ => return HotResult::Err(Val::from("to-instant expects a ZonedDateTime")),
    };

    let epoch_ns = m
        .get(&Val::from("epochNanoseconds"))
        .cloned()
        .unwrap_or(Val::Int(0));
    let mut data = IndexMap::new();
    data.insert(Val::from("epochNanoseconds"), epoch_ns);
    HotResult::Ok(create_temporal_map("Instant", data))
}

// ---------------------------------------------------------------------------
// Pattern-based formatting
// ---------------------------------------------------------------------------

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const MONTH_ABBRS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

// ISO weekday: 1=Monday ... 7=Sunday
const WEEKDAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

const WEEKDAY_ABBRS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Fields extracted from a temporal value for formatting.
struct FormatFields {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    millisecond: u16,
    day_of_week: u16, // 1=Monday...7=Sunday, 0=unknown
    timezone_name: String,
    offset_string: String,
    epoch_seconds: i64,
}

/// Compute ISO day-of-week (1=Mon..7=Sun) from year/month/day using chrono.
fn compute_day_of_week(year: i32, month: u8, day: u8) -> u16 {
    if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month as u32, day as u32) {
        // chrono: Mon=0..Sun=6 for weekday().num_days_from_monday()
        // We want ISO: Mon=1..Sun=7
        (date.weekday().num_days_from_monday() + 1) as u16
    } else {
        0
    }
}

/// Extract FormatFields from a Hot temporal Val map.
fn extract_format_fields(val: &Val) -> Result<FormatFields, String> {
    let untyped = match untype_recursive(val) {
        HotResult::Ok(v) => v,
        _ => val.clone(),
    };

    let m = match &untyped {
        Val::Map(m) => m.as_ref().clone(),
        _ => return Err("format expects a temporal value".to_string()),
    };

    let type_kind = if let Some(Val::Str(t)) = m.get(&Val::from("$type")) {
        match &**t {
            "::hot::time/PlainDate" => "PlainDate",
            "::hot::time/PlainTime" => "PlainTime",
            "::hot::time/PlainDateTime" => "PlainDateTime",
            "::hot::time/ZonedDateTime" => "ZonedDateTime",
            "::hot::time/Instant" => "Instant",
            _ => infer_time_type(&m).unwrap_or("unknown"),
        }
    } else {
        infer_time_type(&m).unwrap_or("unknown")
    };

    let get_i = |key: &str| -> i64 { get_int_field(&m, key) };

    match type_kind {
        "PlainDate" => {
            let year = get_i("year") as i32;
            let month = get_i("month") as u8;
            let day = get_i("day") as u8;
            Ok(FormatFields {
                year,
                month,
                day,
                hour: 0,
                minute: 0,
                second: 0,
                millisecond: 0,
                day_of_week: compute_day_of_week(year, month, day),
                timezone_name: String::new(),
                offset_string: String::new(),
                epoch_seconds: 0,
            })
        }
        "PlainTime" => Ok(FormatFields {
            year: 0,
            month: 0,
            day: 0,
            hour: get_i("hour") as u8,
            minute: get_i("minute") as u8,
            second: get_i("second") as u8,
            millisecond: get_i("millisecond") as u16,
            day_of_week: 0,
            timezone_name: String::new(),
            offset_string: String::new(),
            epoch_seconds: 0,
        }),
        "PlainDateTime" => {
            let year = get_i("year") as i32;
            let month = get_i("month") as u8;
            let day = get_i("day") as u8;
            Ok(FormatFields {
                year,
                month,
                day,
                hour: get_i("hour") as u8,
                minute: get_i("minute") as u8,
                second: get_i("second") as u8,
                millisecond: get_i("millisecond") as u16,
                day_of_week: compute_day_of_week(year, month, day),
                timezone_name: String::new(),
                offset_string: String::new(),
                epoch_seconds: 0,
            })
        }
        "ZonedDateTime" => {
            let year = get_i("year") as i32;
            let month = get_i("month") as u8;
            let day = get_i("day") as u8;
            let epoch_ns = get_i("epochNanoseconds");
            let timezone_name = match m.get(&Val::from("timezone")) {
                Some(Val::Str(s)) => s.to_string(),
                _ => "UTC".to_string(),
            };
            let offset_string = match m.get(&Val::from("offset")) {
                Some(Val::Str(s)) => s.to_string(),
                _ => "+00:00".to_string(),
            };
            Ok(FormatFields {
                year,
                month,
                day,
                hour: get_i("hour") as u8,
                minute: get_i("minute") as u8,
                second: get_i("second") as u8,
                millisecond: get_i("millisecond") as u16,
                day_of_week: compute_day_of_week(year, month, day),
                timezone_name,
                offset_string,
                epoch_seconds: epoch_ns / 1_000_000_000,
            })
        }
        "Instant" => {
            let epoch_ns = get_i("epochNanoseconds");
            let secs = epoch_ns / 1_000_000_000;
            let dt = chrono::Utc
                .timestamp_millis_opt(secs * 1000)
                .single()
                .unwrap_or_else(|| chrono::Utc.timestamp_millis_opt(0).single().unwrap())
                .naive_utc();
            Ok(FormatFields {
                year: dt.year(),
                month: dt.month() as u8,
                day: dt.day() as u8,
                hour: dt.hour() as u8,
                minute: dt.minute() as u8,
                second: dt.second() as u8,
                millisecond: 0,
                day_of_week: (dt.weekday().num_days_from_monday() + 1) as u16,
                timezone_name: "UTC".to_string(),
                offset_string: "+00:00".to_string(),
                epoch_seconds: secs,
            })
        }
        _ => Err(format!("Cannot format value of type '{}'", type_kind)),
    }
}

/// Try to match a token at the start of the character slice. Returns (token_len, replacement).
fn try_match_token(chars: &[char], f: &FormatFields) -> Option<(usize, String)> {
    let remaining: String = chars.iter().collect();

    type TokenFn = fn(&FormatFields) -> String;
    // Tokens ordered longest-first per starting character
    let tokens: &[(&str, TokenFn)] = &[
        ("YYYY", |f| format!("{:04}", f.year)),
        ("YY", |f| format!("{:02}", f.year % 100)),
        ("MMMM", |f| {
            if f.month >= 1 && f.month <= 12 {
                MONTH_NAMES[(f.month - 1) as usize].to_string()
            } else {
                String::new()
            }
        }),
        ("MMM", |f| {
            if f.month >= 1 && f.month <= 12 {
                MONTH_ABBRS[(f.month - 1) as usize].to_string()
            } else {
                String::new()
            }
        }),
        ("MM", |f| format!("{:02}", f.month)),
        ("M", |f| format!("{}", f.month)),
        ("DD", |f| format!("{:02}", f.day)),
        ("D", |f| format!("{}", f.day)),
        ("dddd", |f| {
            if f.day_of_week >= 1 && f.day_of_week <= 7 {
                WEEKDAY_NAMES[(f.day_of_week - 1) as usize].to_string()
            } else {
                String::new()
            }
        }),
        ("ddd", |f| {
            if f.day_of_week >= 1 && f.day_of_week <= 7 {
                WEEKDAY_ABBRS[(f.day_of_week - 1) as usize].to_string()
            } else {
                String::new()
            }
        }),
        ("HH", |f| format!("{:02}", f.hour)),
        ("H", |f| format!("{}", f.hour)),
        ("hh", |f| {
            let h12 = if f.hour == 0 {
                12
            } else if f.hour > 12 {
                f.hour - 12
            } else {
                f.hour
            };
            format!("{:02}", h12)
        }),
        ("h", |f| {
            let h12 = if f.hour == 0 {
                12
            } else if f.hour > 12 {
                f.hour - 12
            } else {
                f.hour
            };
            format!("{}", h12)
        }),
        ("mm", |f| format!("{:02}", f.minute)),
        ("ss", |f| format!("{:02}", f.second)),
        ("SSS", |f| format!("{:03}", f.millisecond)),
        ("A", |f| {
            if f.hour < 12 {
                "AM".to_string()
            } else {
                "PM".to_string()
            }
        }),
        ("a", |f| {
            if f.hour < 12 {
                "am".to_string()
            } else {
                "pm".to_string()
            }
        }),
        ("Z", |f| f.offset_string.clone()),
        ("z", |f| {
            // Timezone abbreviation: derive from timezone name or offset
            timezone_abbreviation(&f.timezone_name, &f.offset_string)
        }),
        ("X", |f| format!("{}", f.epoch_seconds)),
    ];

    for (token, formatter) in tokens {
        if remaining.starts_with(token) {
            return Some((token.len(), formatter(f)));
        }
    }

    None
}

/// Derive a timezone abbreviation from the IANA name and UTC offset.
/// Falls back to the raw offset string when no mapping is known.
///
/// Timezone abbreviations are inherently ambiguous (e.g. CST = US Central,
/// China Standard, or Cuba Standard). We disambiguate by combining the IANA
/// zone name with the current UTC offset, which also implicitly encodes
/// whether DST is active.
fn timezone_abbreviation(tz_name: &str, offset: &str) -> String {
    // ── UTC ──────────────────────────────────────────────────────────
    match tz_name {
        "UTC" | "Etc/UTC" | "Etc/GMT" => return "UTC".to_string(),
        _ => {}
    }

    // ── Americas ─────────────────────────────────────────────────────
    if tz_name.starts_with("America/") || tz_name.starts_with("US/") {
        let is_central = tz_name.contains("Chicago")
            || tz_name.contains("Central")
            || tz_name.contains("Winnipeg")
            || tz_name.contains("Regina")
            || tz_name.contains("Menominee")
            || tz_name.contains("Indiana/Knox")
            || tz_name.contains("Indiana/Tell_City")
            || tz_name.contains("North_Dakota")
            || tz_name.contains("Matamoros")
            || tz_name.contains("Monterrey")
            || tz_name.contains("Mexico_City")
            || tz_name.contains("Merida")
            || tz_name.contains("Bahia_Banderas")
            || tz_name.contains("Rankin_Inlet")
            || tz_name.contains("Resolute");

        let is_mountain = tz_name.contains("Denver")
            || tz_name.contains("Boise")
            || tz_name.contains("Mountain")
            || tz_name.contains("Edmonton")
            || tz_name.contains("Yellowknife")
            || tz_name.contains("Cambridge_Bay")
            || tz_name.contains("Inuvik")
            || tz_name.contains("Ojinaga")
            || tz_name.contains("Chihuahua");

        let is_pacific = tz_name.contains("Los_Angeles")
            || tz_name.contains("Vancouver")
            || tz_name.contains("Pacific")
            || tz_name.contains("Tijuana")
            || tz_name.contains("Dawson")
            || tz_name.contains("Whitehorse");

        let is_alaska = tz_name.contains("Anchorage")
            || tz_name.contains("Juneau")
            || tz_name.contains("Nome")
            || tz_name.contains("Sitka")
            || tz_name.contains("Yakutat")
            || tz_name.contains("Adak");

        let is_atlantic = tz_name.contains("Halifax")
            || tz_name.contains("Atlantic")
            || tz_name.contains("Bermuda")
            || tz_name.contains("Glace_Bay")
            || tz_name.contains("Moncton")
            || tz_name.contains("Thule");

        let is_newfoundland = tz_name.contains("St_Johns") || tz_name.contains("Newfoundland");

        let is_brazil = tz_name.contains("Sao_Paulo")
            || tz_name.contains("Bahia")
            || tz_name.contains("Fortaleza")
            || tz_name.contains("Recife")
            || tz_name.contains("Belem")
            || tz_name.contains("Araguaina");

        let is_argentina = tz_name.contains("Argentina")
            || tz_name.contains("Buenos_Aires")
            || tz_name.contains("Cordoba");

        return match offset {
            // Newfoundland (half-hour offsets, unambiguous)
            "-03:30" if is_newfoundland => "NST".to_string(),
            "-02:30" if is_newfoundland => "NDT".to_string(),
            // -03:00: ADT vs BRT vs ART (guarded, then fallback)
            "-03:00" if is_atlantic => "ADT".to_string(),
            "-03:00" if is_brazil => "BRT".to_string(),
            "-03:00" if is_argentina => "ART".to_string(),
            "-03:00" => offset.to_string(),
            // -02:00: BRST
            "-02:00" if is_brazil => "BRST".to_string(),
            // -04:00: AST vs EDT (Atlantic first, then Eastern daylight)
            "-04:00" if is_atlantic => "AST".to_string(),
            "-04:00" => "EDT".to_string(),
            // -05:00: CDT vs EST (Central daylight vs Eastern standard)
            "-05:00" if is_central => "CDT".to_string(),
            "-05:00" => "EST".to_string(),
            // -06:00: MDT vs CST (Mountain daylight vs Central standard)
            "-06:00" if is_mountain => "MDT".to_string(),
            "-06:00" => "CST".to_string(),
            // -07:00: PDT vs MST (Pacific daylight vs Mountain standard)
            "-07:00" if is_pacific => "PDT".to_string(),
            "-07:00" => "MST".to_string(),
            // -08:00: AKDT vs PST (Alaska daylight vs Pacific standard)
            "-08:00" if is_alaska => "AKDT".to_string(),
            "-08:00" => "PST".to_string(),
            // -09:00: AKST
            "-09:00" if is_alaska => "AKST".to_string(),
            "-09:00" => offset.to_string(),
            // -10:00: HST
            "-10:00" => "HST".to_string(),
            _ => offset.to_string(),
        };
    }

    // Hawaii-Aleutian (Pacific/ prefix)
    if tz_name == "Pacific/Honolulu" || tz_name == "US/Hawaii" {
        return match offset {
            "-10:00" => "HST".to_string(),
            _ => offset.to_string(),
        };
    }

    // ── Europe ───────────────────────────────────────────────────────
    if tz_name.starts_with("Europe/") {
        let is_eastern = tz_name.contains("Athens")
            || tz_name.contains("Bucharest")
            || tz_name.contains("Helsinki")
            || tz_name.contains("Kiev")
            || tz_name.contains("Kyiv")
            || tz_name.contains("Riga")
            || tz_name.contains("Sofia")
            || tz_name.contains("Tallinn")
            || tz_name.contains("Vilnius");

        let is_moscow = tz_name.contains("Moscow")
            || tz_name.contains("Minsk")
            || tz_name.contains("Kirov")
            || tz_name.contains("Simferopol");

        let is_western =
            tz_name.contains("Lisbon") || tz_name.contains("Azores") || tz_name.contains("Canary");

        let is_london =
            tz_name.contains("London") || tz_name.contains("Dublin") || tz_name.contains("Belfast");

        return match offset {
            // Western European Time
            "+00:00" if is_western => "WET".to_string(),
            "+01:00" if is_western => "WEST".to_string(),
            // GMT / BST
            "+00:00" if is_london => "GMT".to_string(),
            "+01:00" if is_london => "BST".to_string(),
            "+00:00" => "GMT".to_string(),
            // Central European Time
            "+01:00" => "CET".to_string(),
            "+02:00" if is_eastern => "EET".to_string(),
            "+02:00" => "CEST".to_string(),
            // Eastern European Time
            "+03:00" if is_moscow => "MSK".to_string(),
            "+03:00" if is_eastern => "EEST".to_string(),
            "+03:00" => "MSK".to_string(),
            _ => offset.to_string(),
        };
    }

    // ── Asia ─────────────────────────────────────────────────────────
    if tz_name.starts_with("Asia/") {
        let is_singapore = tz_name.contains("Singapore");
        let is_hong_kong = tz_name.contains("Hong_Kong");
        let is_manila = tz_name.contains("Manila");
        let is_kuala_lumpur = tz_name.contains("Kuala_Lumpur") || tz_name.contains("Kuching");
        let is_seoul = tz_name.contains("Seoul");
        let is_kolkata = tz_name.contains("Kolkata") || tz_name.contains("Calcutta");
        let is_karachi = tz_name.contains("Karachi");
        let is_dubai = tz_name.contains("Dubai") || tz_name.contains("Muscat");
        let is_bangkok = tz_name.contains("Bangkok")
            || tz_name.contains("Ho_Chi_Minh")
            || tz_name.contains("Saigon")
            || tz_name.contains("Phnom_Penh")
            || tz_name.contains("Vientiane");
        let is_jakarta = tz_name.contains("Jakarta") || tz_name.contains("Pontianak");

        return match offset {
            "+09:00" if is_seoul => "KST".to_string(),
            "+09:00" => "JST".to_string(),
            "+08:00" if is_singapore => "SGT".to_string(),
            "+08:00" if is_hong_kong => "HKT".to_string(),
            "+08:00" if is_manila => "PHT".to_string(),
            "+08:00" if is_kuala_lumpur => "MYT".to_string(),
            "+08:00" => "CST".to_string(), // China Standard Time (default)
            "+07:00" if is_jakarta => "WIB".to_string(),
            "+07:00" if is_bangkok => "ICT".to_string(),
            "+07:00" => "ICT".to_string(),
            "+05:30" if is_kolkata => "IST".to_string(),
            "+05:30" => "IST".to_string(),
            "+05:00" if is_karachi => "PKT".to_string(),
            "+05:00" => "PKT".to_string(),
            "+04:00" if is_dubai => "GST".to_string(),
            "+04:00" => "GST".to_string(),
            "+03:30" => "IRST".to_string(), // Iran Standard Time
            "+04:30" => "IRDT".to_string(), // Iran Daylight Time
            _ => offset.to_string(),
        };
    }

    // ── Australia ────────────────────────────────────────────────────
    if tz_name.starts_with("Australia/") {
        let is_central = tz_name.contains("Adelaide")
            || tz_name.contains("Darwin")
            || tz_name.contains("Broken_Hill");
        let is_western = tz_name.contains("Perth");

        return match offset {
            "+11:00" => "AEDT".to_string(),
            "+10:00" if !is_central && !is_western => "AEST".to_string(),
            "+10:30" if is_central => "ACDT".to_string(),
            "+09:30" if is_central => "ACST".to_string(),
            "+08:00" if is_western => "AWST".to_string(),
            _ => offset.to_string(),
        };
    }

    // ── Pacific ──────────────────────────────────────────────────────
    if tz_name.starts_with("Pacific/") {
        return match offset {
            "+13:00" if tz_name.contains("Auckland") || tz_name.contains("Fiji") => {
                "NZDT".to_string()
            }
            "+12:00" if tz_name.contains("Auckland") => "NZST".to_string(),
            "-10:00" if tz_name.contains("Honolulu") => "HST".to_string(),
            _ => offset.to_string(),
        };
    }

    // ── Africa ───────────────────────────────────────────────────────
    if tz_name.starts_with("Africa/") {
        let is_south_africa = tz_name.contains("Johannesburg");
        let is_east_africa = tz_name.contains("Nairobi")
            || tz_name.contains("Addis_Ababa")
            || tz_name.contains("Dar_es_Salaam")
            || tz_name.contains("Kampala")
            || tz_name.contains("Mogadishu");

        return match offset {
            "+02:00" if is_south_africa => "SAST".to_string(),
            "+02:00" => "CAT".to_string(),
            "+03:00" if is_east_africa => "EAT".to_string(),
            "+03:00" => "EAT".to_string(),
            "+01:00" => "WAT".to_string(),
            _ => offset.to_string(),
        };
    }

    offset.to_string()
}

/// Format a temporal value using a pattern string.
/// Supports: YYYY, YY, MMMM, MMM, MM, M, DD, D, dddd, ddd,
///           HH, H, hh, h, mm, ss, SSS, A, a, Z, z, X
/// Literal text can be escaped with square brackets: [text]
pub fn format_temporal(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::time/format", args, 2);

    let pattern = match &args[1] {
        Val::Str(s) => s.to_string(),
        _ => {
            return HotResult::Err(Val::from(
                "format: second argument must be a pattern string",
            ));
        }
    };

    let fields = match extract_format_fields(&args[0]) {
        Ok(f) => f,
        Err(e) => return HotResult::Err(Val::from(format!("format: {}", e))),
    };

    let chars: Vec<char> = pattern.chars().collect();
    let mut result = String::new();
    let mut i = 0;

    while i < chars.len() {
        // Escaped literal: [...]
        if chars[i] == '[' {
            i += 1;
            while i < chars.len() && chars[i] != ']' {
                result.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1; // skip ']'
            }
            continue;
        }

        // Try to match a token
        if let Some((token_len, replacement)) = try_match_token(&chars[i..], &fields) {
            result.push_str(&replacement);
            i += token_len;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    HotResult::Ok(Val::from(result))
}
