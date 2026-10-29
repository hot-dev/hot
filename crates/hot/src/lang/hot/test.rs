use crate::lang::hot::time;
use crate::lang::hot::r#type::{HotResult, untype_recursive};
use crate::val::Val;
use crate::validate_args;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

/// Print a value with a newline (uses io::write_stdout for capture support)
pub fn println(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        1 => {
            let val = &args[0];
            let output = format_val_for_println(val);
            // Use write_stdout to respect capture mode (for LSP)
            let _ = super::io::write_stdout(&format!("{}\n", output));
            HotResult::Ok(Val::from(format!("{}\n", output)))
        }
        _ => HotResult::Err(Val::from("println expects 1 argument")),
    }
}

/// Format a value for println output (without quotes for strings)
fn format_val_for_println(val: &Val) -> String {
    use crate::val::Val;

    match val {
        Val::Null => "null".to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Int(i) => i.to_string(),
        Val::Dec(d) => d.to_string(),
        Val::Str(s) => (**s).to_owned(), // No quotes for println
        Val::Vec(v) => {
            let items: Vec<String> = v.iter().map(format_val_for_println).collect();
            format!("[{}]", items.join(", "))
        }
        Val::Map(m) => {
            let items: Vec<String> = m
                .iter()
                .map(|(k, v)| {
                    // For maps in println, use unquoted keys like {a: 1}
                    let key_str = match k {
                        Val::Str(s) => (**s).to_owned(),
                        _ => format_val_for_println(k),
                    };
                    format!("{}: {}", key_str, format_val_for_println(v))
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        Val::Box(b) => format!("Box({})", b.to_string()),
        Val::Byte(b) => b.to_string(),
        Val::Bytes(bytes) => format!("{:?}", bytes),
    }
}

// ---------------------------------------------------------------------------
// Test harness: the clock a test can fix, the events sent in a test, and the
// schedules they arm. `hot test` installs one per run; nothing else does, so
// production runs read the system clock and `send` publishes.
// ---------------------------------------------------------------------------

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// State shared by every thread of one test run.
#[derive(Default)]
pub struct TestHarness {
    /// Fixed time in nanoseconds since the Unix epoch; `None` reads the system clock.
    now: Mutex<Option<i128>>,
    /// Events sent since the last `take-sent`, as `{type, data}` maps.
    sent: Mutex<Vec<Val>>,
    /// Schedules armed by `hot:schedule:new`, in the order they were armed.
    schedules: Mutex<Vec<ArmedSchedule>>,
}

struct ArmedSchedule {
    id: String,
    function: String,
    args: Val,
    /// Next due time in nanoseconds since the Unix epoch.
    at: i128,
    /// Recurring schedules re-arm at their next occurrence after firing.
    cron: Option<croner::Cron>,
}

impl ArmedSchedule {
    fn to_val(&self) -> Val {
        crate::val!({
            "id": self.id.clone(),
            "fn": self.function.clone(),
            "args": self.args.clone(),
            "at": time::instant_val(self.at)
        })
    }
}

thread_local! {
    static THREAD_HARNESS: RefCell<Option<Arc<TestHarness>>> = const { RefCell::new(None) };
}

/// Installs a harness for this thread, restored on return or unwind. Worker
/// threads that run branches of the same test (`parallel`, `pmap`) install
/// the parent's harness so they share its clock and events.
pub struct ThreadHarnessGuard {
    previous: Option<Arc<TestHarness>>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ThreadHarnessGuard {
    pub fn install(harness: Arc<TestHarness>) -> Self {
        Self {
            previous: THREAD_HARNESS.with(|slot| slot.replace(Some(harness))),
            _thread_bound: PhantomData,
        }
    }

    /// Install `harness` when there is one.
    pub fn install_some(harness: Option<Arc<TestHarness>>) -> Option<Self> {
        harness.map(Self::install)
    }
}

impl Drop for ThreadHarnessGuard {
    fn drop(&mut self) {
        THREAD_HARNESS.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

/// The harness of the test run on this thread, if any.
pub fn current_harness() -> Option<Arc<TestHarness>> {
    THREAD_HARNESS.with(|slot| slot.borrow().clone())
}

/// The time a test fixed, in nanoseconds since the Unix epoch.
pub fn fixed_now() -> Option<i128> {
    THREAD_HARNESS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|harness| *harness.now.lock())
    })
}

/// Keep an event a test run sent, for `::hot::test/deliver`. Returns false
/// outside a test run, where `send` publishes instead.
pub fn record_sent(event_type: &str, data: &Val) -> bool {
    match current_harness() {
        Some(harness) => {
            harness
                .sent
                .lock()
                .push(crate::val!({"type": event_type, "data": data.clone()}));
            true
        }
        None => false,
    }
}

fn harness_for(name: &str) -> Result<Arc<TestHarness>, Val> {
    current_harness().ok_or_else(|| Val::from(format!("{name} works only in a `hot test` run")))
}

fn harness_now(harness: &TestHarness) -> Result<i128, String> {
    match *harness.now.lock() {
        Some(nanos) => Ok(nanos),
        None => time::system_epoch_nanos(),
    }
}

/// Nanoseconds since the Unix epoch of an RFC 3339 string, Instant or ZonedDateTime.
fn epoch_nanos_of(value: &Val) -> Result<i128, String> {
    if let Val::Str(text) = value {
        return chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .and_then(|at| at.timestamp_nanos_opt())
            .map(i128::from)
            .ok_or_else(|| {
                format!(
                    "expected an RFC 3339 time such as \"2026-10-14T09:00:00Z\", got \"{text}\""
                )
            });
    }
    let untyped = match untype_recursive(value) {
        HotResult::Ok(v) => v,
        _ => value.clone(),
    };
    match &untyped {
        Val::Map(m) => match m.get(&Val::from("epochNanoseconds")) {
            Some(Val::Int(nanos)) => Ok(i128::from(*nanos)),
            _ => Err("expected an Instant or ZonedDateTime".to_string()),
        },
        other => Err(format!("expected a time, got {other}")),
    }
}

/// Nanoseconds in a Duration of weeks, days and smaller units. Years and
/// months have no fixed length, so they are refused.
fn duration_nanos(value: &Val) -> Result<i128, String> {
    let untyped = match untype_recursive(value) {
        HotResult::Ok(v) => v,
        _ => value.clone(),
    };
    let duration = time::val_to_duration(&untyped)?;
    if duration.years() != 0 || duration.months() != 0 {
        return Err(
            "years and months have no fixed length; use weeks, days or smaller units".into(),
        );
    }
    let seconds = i128::from(duration.weeks()) * 7 * 86_400
        + i128::from(duration.days()) * 86_400
        + i128::from(duration.hours()) * 3_600
        + i128::from(duration.minutes()) * 60
        + i128::from(duration.seconds());
    Ok(seconds * NANOS_PER_SECOND
        + i128::from(duration.milliseconds()) * 1_000_000
        + duration.microseconds() * 1_000
        + duration.nanoseconds())
}

/// Fix the test clock at a time; `::hot::time/now` returns it until changed.
pub fn set_now(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/set-now", args, 1);
    let harness = match harness_for("::hot::test/set-now") {
        Ok(harness) => harness,
        Err(e) => return HotResult::Err(e),
    };
    match epoch_nanos_of(&args[0]) {
        Ok(nanos) => {
            *harness.now.lock() = Some(nanos);
            HotResult::Ok(time::instant_val(nanos))
        }
        Err(e) => HotResult::Err(Val::from(format!("::hot::test/set-now: {e}"))),
    }
}

/// Move the test clock by a Duration, fixing it first at the system time if
/// no test fixed it.
pub fn advance(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/advance", args, 1);
    let harness = match harness_for("::hot::test/advance") {
        Ok(harness) => harness,
        Err(e) => return HotResult::Err(e),
    };
    let step = match duration_nanos(&args[0]) {
        Ok(step) => step,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::test/advance: {e}"))),
    };
    let mut now = harness.now.lock();
    let from = match *now {
        Some(nanos) => nanos,
        None => match time::system_epoch_nanos() {
            Ok(nanos) => nanos,
            Err(e) => return HotResult::Err(Val::from(e)),
        },
    };
    *now = Some(from + step);
    HotResult::Ok(time::instant_val(from + step))
}

/// Return the test clock to the system clock.
pub fn clear_now(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/clear-now", args, 0);
    match harness_for("::hot::test/clear-now") {
        Ok(harness) => {
            *harness.now.lock() = None;
            HotResult::Ok(Val::Null)
        }
        Err(e) => HotResult::Err(e),
    }
}

/// The time a test fixed, or null when `now` reads the system clock.
pub fn fixed_now_fn(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/fixed-now", args, 0);
    match harness_for("::hot::test/fixed-now") {
        Ok(harness) => HotResult::Ok(match *harness.now.lock() {
            Some(nanos) => time::instant_val(nanos),
            None => Val::Null,
        }),
        Err(e) => HotResult::Err(e),
    }
}

/// Clear the clock, the sent events and the armed schedules between tests.
pub fn reset(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/reset", args, 0);
    match harness_for("::hot::test/reset") {
        Ok(harness) => {
            *harness.now.lock() = None;
            harness.sent.lock().clear();
            harness.schedules.lock().clear();
            HotResult::Ok(Val::Null)
        }
        Err(e) => HotResult::Err(e),
    }
}

/// The events sent since the last call, oldest first.
pub fn take_sent(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/take-sent", args, 0);
    match harness_for("::hot::test/take-sent") {
        Ok(harness) => HotResult::Ok(Val::Vec(std::mem::take(&mut *harness.sent.lock()))),
        Err(e) => HotResult::Err(e),
    }
}

/// The next occurrence of `cron` strictly after `after_nanos`.
fn next_occurrence(cron: &croner::Cron, after_nanos: i128) -> Result<i128, String> {
    let after = chrono::DateTime::from_timestamp_nanos(after_nanos as i64);
    cron.find_next_occurrence(&after, false)
        .map_err(|e| format!("no next occurrence: {e}"))?
        .timestamp_nanos_opt()
        .map(i128::from)
        .ok_or_else(|| "next occurrence is out of range".to_string())
}

/// The schedule a `hot:schedule:new` event's data arms at `now`.
fn armed_schedule(data: &Val, now: i128) -> Result<ArmedSchedule, String> {
    let Val::Map(data) = data else {
        return Err("expected the event data map {fn, args, schedule}".into());
    };
    let function =
        super::schedule::function_name(data.get(&Val::from("fn")).unwrap_or(&Val::Null))?;
    let args = match data.get(&Val::from("args")) {
        None | Some(Val::Null) => Val::Vec(Vec::new()),
        Some(args @ Val::Vec(_)) => args.clone(),
        Some(other) => return Err(format!("expected Vec for args, got {other}")),
    };
    let Some(Val::Str(expr)) = data.get(&Val::from("schedule")) else {
        return Err("expected a schedule string".into());
    };
    let from = chrono::DateTime::from_timestamp_nanos(now as i64);
    let (at, cron) = match crate::db::parse_schedule_expression_at(expr, from)? {
        crate::db::ScheduleType::At(at) => (
            at.timestamp_nanos_opt()
                .map(i128::from)
                .ok_or("schedule time is out of range")?,
            None,
        ),
        crate::db::ScheduleType::Cron(expr) => {
            let cron = crate::db::parse_cron(&expr)
                .map_err(|e| format!("could not parse '{expr}' as cron: {e}"))?;
            (next_occurrence(&cron, now)?, Some(cron))
        }
    };
    Ok(ArmedSchedule {
        id: uuid::Uuid::now_v7().to_string(),
        function,
        args,
        at,
        cron,
    })
}

/// Arm a schedule from a `hot:schedule:new` event's data, as the scheduler
/// would, with relative and recurring times measured from the test clock.
pub fn arm_schedule(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/arm-schedule", args, 1);
    let prefix = "::hot::test/arm-schedule";
    let harness = match harness_for(prefix) {
        Ok(harness) => harness,
        Err(e) => return HotResult::Err(e),
    };
    let armed = harness_now(&harness).and_then(|now| armed_schedule(&args[0], now));
    match armed {
        Ok(armed) => {
            let id = Val::from(armed.id.clone());
            harness.schedules.lock().push(armed);
            HotResult::Ok(id)
        }
        Err(e) => HotResult::Err(Val::from(format!("{prefix}: {e}"))),
    }
}

/// Cancel armed schedules from a `hot:schedule:cancel` event's data: by
/// `schedule-id`, or every schedule of `fn`. Returns whether any was armed.
pub fn cancel_schedule(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/cancel-schedule", args, 1);
    let prefix = "::hot::test/cancel-schedule";
    let harness = match harness_for(prefix) {
        Ok(harness) => harness,
        Err(e) => return HotResult::Err(e),
    };
    let Val::Map(data) = &args[0] else {
        return HotResult::Err(Val::from(format!(
            "{prefix}: expected the event data map {{schedule-id}} or {{fn}}"
        )));
    };
    let mut schedules = harness.schedules.lock();
    let before = schedules.len();
    if let Some(Val::Str(id)) = data.get(&Val::from("schedule-id")) {
        schedules.retain(|s| s.id != **id);
    } else if let Some(function) = data.get(&Val::from("fn")) {
        match super::schedule::function_name(function) {
            Ok(function) => schedules.retain(|s| s.function != function),
            Err(e) => return HotResult::Err(Val::from(format!("{prefix}: {e}"))),
        }
    } else {
        return HotResult::Err(Val::from(format!("{prefix}: expected schedule-id or fn")));
    }
    HotResult::Ok(Val::Bool(schedules.len() < before))
}

/// The armed schedules, soonest first.
pub fn schedules(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/schedules", args, 0);
    match harness_for("::hot::test/schedules") {
        Ok(harness) => {
            let schedules = harness.schedules.lock();
            let mut order: Vec<&ArmedSchedule> = schedules.iter().collect();
            order.sort_by_key(|s| s.at);
            HotResult::Ok(Val::Vec(
                order.into_iter().map(ArmedSchedule::to_val).collect(),
            ))
        }
        Err(e) => HotResult::Err(e),
    }
}

/// Take the soonest schedule due at or before `until` (an Instant), or null.
/// A one-time schedule is disarmed; a recurring one re-arms at its next
/// occurrence after this one.
pub fn take_due(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::test/take-due", args, 1);
    let prefix = "::hot::test/take-due";
    let harness = match harness_for(prefix) {
        Ok(harness) => harness,
        Err(e) => return HotResult::Err(e),
    };
    let until = match epoch_nanos_of(&args[0]) {
        Ok(until) => until,
        Err(e) => return HotResult::Err(Val::from(format!("{prefix}: {e}"))),
    };
    let mut schedules = harness.schedules.lock();
    // Soonest first; among equal times, the one armed first.
    let Some(index) = schedules
        .iter()
        .enumerate()
        .filter(|(_, s)| s.at <= until)
        .min_by_key(|(index, s)| (s.at, *index))
        .map(|(index, _)| index)
    else {
        return HotResult::Ok(Val::Null);
    };
    let due = schedules[index].to_val();
    let next = match &schedules[index].cron {
        Some(cron) => match next_occurrence(cron, schedules[index].at) {
            Ok(next) => Some(next),
            Err(e) => return HotResult::Err(Val::from(format!("{prefix}: {e}"))),
        },
        None => None,
    };
    match next {
        Some(next) => schedules[index].at = next,
        None => {
            schedules.remove(index);
        }
    }
    HotResult::Ok(due)
}

#[cfg(test)]
mod harness_tests {
    use super::*;

    fn ok(result: HotResult<Val>) -> Val {
        match result {
            HotResult::Ok(value) => value,
            HotResult::Err(error) => panic!("{error}"),
        }
    }

    fn instant_nanos(value: &Val) -> i128 {
        epoch_nanos_of(value).unwrap()
    }

    #[test]
    fn without_a_harness_the_clock_is_real_and_cannot_be_fixed() {
        assert!(current_harness().is_none());
        assert!(matches!(
            set_now(&[Val::from("2001-02-03T04:05:06Z")]),
            HotResult::Err(_)
        ));
        assert!(matches!(
            advance(&[crate::val!({"hours": 1})]),
            HotResult::Err(_)
        ));
        assert!(
            !record_sent("t:event", &Val::Null),
            "send publishes outside a test run"
        );
        let real = instant_nanos(&ok(time::now(&[])));
        assert!(real > 1_700_000_000 * NANOS_PER_SECOND);
    }

    #[test]
    fn a_harness_fixes_moves_and_clears_the_clock_and_restores_on_drop() {
        let fixed = 981_173_106 * NANOS_PER_SECOND;
        {
            let _harness = ThreadHarnessGuard::install(Arc::new(TestHarness::default()));
            ok(set_now(&[Val::from("2001-02-03T04:05:06Z")]));
            assert_eq!(instant_nanos(&ok(time::now(&[]))), fixed);
            ok(advance(&[crate::val!({"days": 1, "minutes": 2})]));
            assert_eq!(
                instant_nanos(&ok(time::now(&[]))),
                fixed + (86_400 + 120) * NANOS_PER_SECOND
            );
            assert!(matches!(
                advance(&[crate::val!({"months": 1})]),
                HotResult::Err(_)
            ));

            // A worker thread given the harness reads the same clock
            let shared = current_harness();
            let seen = std::thread::spawn(move || {
                let _worker = ThreadHarnessGuard::install_some(shared);
                fixed_now()
            })
            .join()
            .unwrap();
            assert_eq!(seen, fixed_now());

            ok(clear_now(&[]));
            assert_eq!(fixed_now(), None);
            ok(set_now(&[Val::from("2001-02-03T04:05:06Z")]));
        }
        assert!(current_harness().is_none(), "the guard restores the thread");
        assert_eq!(fixed_now(), None);
    }

    #[test]
    fn schedules_fall_due_by_the_test_clock() {
        let _harness = ThreadHarnessGuard::install(Arc::new(TestHarness::default()));
        ok(set_now(&[Val::from("2026-10-14T09:00:00Z")]));
        let once = crate::val!({"fn": "::app/check", "args": ["a"], "schedule": "in 2 hours"});
        let daily = crate::val!({"fn": "::app/daily", "schedule": "0 0 9 * * *"});
        ok(arm_schedule(&[once]));
        ok(arm_schedule(&[daily]));

        let until = time::instant_val(instant_nanos(&Val::from("2026-10-15T10:00:00Z")));
        let first = ok(take_due(std::slice::from_ref(&until)));
        assert_eq!(first.get("fn"), Some(Val::from("::app/check")));
        assert_eq!(
            instant_nanos(&first.get("at").unwrap()),
            instant_nanos(&Val::from("2026-10-14T11:00:00Z"))
        );
        let second = ok(take_due(std::slice::from_ref(&until)));
        assert_eq!(second.get("fn"), Some(Val::from("::app/daily")));
        assert_eq!(ok(take_due(std::slice::from_ref(&until))), Val::Null);

        let armed = ok(schedules(&[]));
        let Val::Vec(armed) = armed else {
            panic!("schedules returns a Vec")
        };
        assert_eq!(armed.len(), 1, "the daily schedule re-armed");
        assert!(ok(cancel_schedule(&[crate::val!({"fn": "::app/daily"})])) == Val::Bool(true));
        ok(reset(&[]));
        assert_eq!(fixed_now(), None);
    }
}
