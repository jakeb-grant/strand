//! The `clock` and `calendar` services, and `Date` methods.
//!
//! The clock keeps two signals, the wall-clock minute and second.
//! `clock.format("%H:%M")` reads only the minute, so the hello bar wakes
//! once a minute; a pattern showing seconds reads the second signal, and
//! the host loop is asked to wake every second only while something reads
//! it ([`Clock::next_wake`]). Time comes from the host loop
//! ([`Clock::set_time`]), so tests drive a fake clock, and `strand run`
//! sleeps for [`Clock::next_wake`] on a `CLOCK_REALTIME` timerfd armed at
//! that absolute time with `TFD_TIMER_CANCEL_ON_SET` (`crates/strand/
//! src/run.rs`), so a resume or a clock step shows the new time at once.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Datelike, FixedOffset, Local, Months, NaiveDate, Utc};
use strand_core::{Error, Runtime, Signal};

use super::value::Value;
use crate::ty::{RecordId, TypeTable};

/// Which wall-clock zone dates and `format` use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// The system's local time zone.
    Local,
    /// A fixed offset (tests: deterministic everywhere).
    Fixed(FixedOffset),
}

/// The clock service.
#[derive(Debug)]
pub struct Clock {
    zone: Zone,
    /// Unix seconds, floored to the minute.
    minute: Signal<i64>,
    /// Unix seconds.
    second: Signal<i64>,
    date: Option<RecordId>,
    day: Option<RecordId>,
}

fn unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs_f64().ceil() as i64),
    }
}

fn fail(msg: impl Into<String>) -> Error {
    Error::failed(msg.into())
}

/// True if `pattern` shows seconds (so a binding using it must re-run
/// every second). Reads chrono's own items, so padding flags (`%-S`,
/// `%_S`, `%0S`), fractions (`%f`, `%.3f`, `%3f`) and composite specs
/// (`%T`, `%X`, `%c`, `%r`, `%+`) all count.
pub fn shows_seconds(pattern: &str) -> bool {
    use chrono::format::{Fixed, Numeric};
    StrftimeItems::new(pattern).any(|i| match i {
        Item::Numeric(n, _) => matches!(
            n,
            Numeric::Second | Numeric::Timestamp | Numeric::Nanosecond
        ),
        Item::Fixed(f) => matches!(
            f,
            Fixed::Nanosecond
                | Fixed::Nanosecond3
                | Fixed::Nanosecond6
                | Fixed::Nanosecond9
                | Fixed::RFC2822
                | Fixed::RFC3339
                // `%3f` and friends (no public name).
                | Fixed::Internal(_)
        ),
        _ => false,
    })
}

/// `strftime` with chrono, without panicking on a bad pattern.
fn format_with<D>(d: &D, pattern: &str) -> Result<String, Error>
where
    D: FormatItems,
{
    let items: Vec<Item<'_>> = StrftimeItems::new(pattern).collect();
    if items.iter().any(|i| matches!(i, Item::Error)) {
        return Err(fail(format!("`{pattern}` is not a valid time pattern")));
    }
    let mut out = String::new();
    write!(out, "{}", d.with_items(items.into_iter()))
        .map_err(|_| fail(format!("`{pattern}` cannot format this value")))?;
    Ok(out)
}

/// The chrono values `format` works on.
trait FormatItems {
    fn with_items<'a>(
        &self,
        items: std::vec::IntoIter<Item<'a>>,
    ) -> chrono::format::DelayedFormat<std::vec::IntoIter<Item<'a>>>;
}

impl<Tz: chrono::TimeZone> FormatItems for DateTime<Tz>
where
    Tz::Offset: std::fmt::Display,
{
    fn with_items<'a>(
        &self,
        items: std::vec::IntoIter<Item<'a>>,
    ) -> chrono::format::DelayedFormat<std::vec::IntoIter<Item<'a>>> {
        self.format_with_items(items)
    }
}

impl FormatItems for NaiveDate {
    fn with_items<'a>(
        &self,
        items: std::vec::IntoIter<Item<'a>>,
    ) -> chrono::format::DelayedFormat<std::vec::IntoIter<Item<'a>>> {
        self.format_with_items(items)
    }
}

impl Clock {
    pub fn new(rt: &Runtime, types: &TypeTable, zone: Zone, now: SystemTime) -> Clock {
        let s = unix(now);
        let minute = rt.signal(s.div_euclid(60) * 60);
        let second = rt.signal(s);
        rt.set_name(minute.id(), "clock.minute");
        rt.set_name(second.id(), "clock.second");
        Clock {
            zone,
            minute,
            second,
            date: types.find_record("Date"),
            day: types.find_record("Day"),
        }
    }

    /// The clock's nodes (the minute and the second): what a read of
    /// `clock` or `calendar` can depend on.
    pub fn ids(&self) -> [strand_core::NodeId; 2] {
        [self.minute.id(), self.second.id()]
    }

    /// The wall clock reached `now`.
    pub fn set_time(&self, rt: &Runtime, now: SystemTime) {
        let s = unix(now);
        let _ = self.minute.set(rt, s.div_euclid(60) * 60);
        let _ = self.second.set(rt, s);
    }

    /// The next boundary the clock must be woken at: the next second
    /// while a binding reads seconds, else the next minute while one reads
    /// the time at all, else never (a bar without a clock sleeps).
    pub fn next_wake(&self, rt: &Runtime) -> Option<SystemTime> {
        let read = |s: Signal<i64>| rt.observers(s.id()).is_ok_and(|o| !o.is_empty());
        let now = self.second.get_untracked(rt).ok()?;
        let next = if read(self.second) {
            now + 1
        } else if read(self.minute) {
            now.div_euclid(60) * 60 + 60
        } else {
            return None;
        };
        Some(UNIX_EPOCH + Duration::from_secs(next.max(0) as u64))
    }

    fn datetime(&self, secs: i64) -> Option<DateTime<FixedOffset>> {
        let utc: DateTime<Utc> = DateTime::from_timestamp(secs, 0)?;
        Some(match self.zone {
            Zone::Fixed(off) => utc.with_timezone(&off),
            Zone::Local => {
                let local = utc.with_timezone(&Local);
                local.with_timezone(local.offset())
            }
        })
    }

    fn today_at(&self, secs: i64) -> Option<NaiveDate> {
        self.datetime(secs).map(|d| d.date_naive())
    }

    /// `clock.today` / `clock.now` (a `Date`), tracked on the minute.
    pub fn read(&self, rt: &Runtime, field: &str) -> Result<Value, Error> {
        match field {
            "today" | "now" => {
                let m = self.minute.get(rt)?;
                let d = self
                    .today_at(m)
                    .ok_or_else(|| fail("the clock is out of range"))?;
                Ok(self.date_value(d))
            }
            _ => Err(fail(format!("`clock` has no field `{field}`"))),
        }
    }

    /// `clock.format(pattern)`: wakes only as often as the pattern
    /// changes.
    pub fn format(&self, rt: &Runtime, pattern: &str) -> Result<Value, Error> {
        let secs = if shows_seconds(pattern) {
            self.second.get(rt)?
        } else {
            self.minute.get(rt)?
        };
        let dt = self
            .datetime(secs)
            .ok_or_else(|| fail("the clock is out of range"))?;
        Ok(Value::text(format_with(&dt, pattern)?))
    }

    /// `calendar.days(month)`: six weeks from the Monday on or before the
    /// first of `month`'s month, each a `Day` keyed by its date.
    pub fn days(&self, rt: &Runtime, types: &TypeTable, month: &Value) -> Result<Value, Error> {
        let first = date_of(types, month)
            .and_then(|d| d.with_day(1))
            .ok_or_else(|| fail("`calendar.days` needs a date"))?;
        let today = self.today_at(self.minute.get(rt)?);
        let offset = first.weekday().num_days_from_monday() as u64;
        let start = first
            .checked_sub_days(chrono::Days::new(offset))
            .ok_or_else(|| fail("date out of range"))?;
        let Some(day) = self.day else {
            return Ok(Value::list(Vec::new()));
        };
        let mut out = Vec::with_capacity(42);
        for i in 0..42 {
            let Some(d) = start.checked_add_days(chrono::Days::new(i)) else {
                break;
            };
            let weekday = d.weekday().num_days_from_monday();
            out.push(Value::record(
                day,
                vec![
                    self.date_value(d),
                    Value::Bool(d.month() == first.month() && d.year() == first.year()),
                    Value::Bool(Some(d) == today),
                    Value::Bool(weekday >= 5),
                ],
            ));
        }
        Ok(Value::list(out))
    }

    fn date_value(&self, d: NaiveDate) -> Value {
        match self.date {
            Some(r) => date_record(r, d),
            None => Value::Null,
        }
    }
}

/// A `Date` record: year, month, day, weekday (Monday 1 … Sunday 7).
pub fn date_record(r: RecordId, d: NaiveDate) -> Value {
    Value::record(
        r,
        vec![
            Value::int(d.year() as i64),
            Value::int(d.month() as i64),
            Value::int(d.day() as i64),
            Value::int(d.weekday().number_from_monday() as i64),
        ],
    )
}

/// The date a `Date` record holds.
pub fn date_of(types: &TypeTable, v: &Value) -> Option<NaiveDate> {
    let n = |f: &str| v.field(types, f).and_then(Value::as_f64);
    NaiveDate::from_ymd_opt(n("year")? as i32, n("month")? as u32, n("day")? as u32)
}

/// `d.month_start()`, `d.add(months: -1)`, `d.format("%B %Y")`.
pub(crate) fn date_method(
    types: &TypeTable,
    recv: &Value,
    name: &str,
    args: &[Value],
) -> Result<Value, Error> {
    let (Some(d), Some(Value::Record(r))) = (date_of(types, recv), Some(recv)) else {
        return Err(fail("not a date"));
    };
    let arg = |i: usize| args.get(i).and_then(Value::as_f64).unwrap_or(0.0) as i64;
    let out_of_range = || fail("date out of range");
    match name {
        "month_start" => d
            .with_day(1)
            .map(|d| date_record(r.ty, d))
            .ok_or_else(|| fail("date out of range")),
        "add" => {
            // Config values: checked, never an overflow panic or a
            // silently truncated month count.
            let months = arg(2)
                .checked_mul(12)
                .and_then(|y| y.checked_add(arg(1)))
                .ok_or_else(out_of_range)?;
            let count = u32::try_from(months.unsigned_abs()).map_err(|_| out_of_range())?;
            let shifted = if months >= 0 {
                d.checked_add_months(Months::new(count))
            } else {
                d.checked_sub_months(Months::new(count))
            };
            let days = arg(0);
            shifted
                .and_then(|d| {
                    if days >= 0 {
                        d.checked_add_days(chrono::Days::new(days as u64))
                    } else {
                        d.checked_sub_days(chrono::Days::new(days.unsigned_abs()))
                    }
                })
                .map(|d| date_record(r.ty, d))
                .ok_or_else(|| fail("date out of range"))
        }
        "format" => {
            let p = args.first().and_then(Value::as_text).unwrap_or("");
            Ok(Value::text(format_with(&d, p)?))
        }
        _ => Err(fail(format!("`Date` has no method `{name}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> TypeTable {
        crate::schema::Schema::builtin().types.clone()
    }

    fn at(s: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(s as u64)
    }

    #[test]
    fn seconds_are_found_through_padding_and_composites() {
        for p in [
            "%S",
            "%-S",
            "%_S",
            "%0S",
            "%T",
            "%X",
            "%c",
            "%r",
            "%+",
            "%s",
            "%f",
            "%.3f",
            "%3f",
            "%H:%M:%-S",
        ] {
            assert!(shows_seconds(p), "{p}");
        }
        for p in ["%H:%M", "%-H:%M", "%a %d %b", "%p", "%%S", "%R", "%D"] {
            assert!(!shows_seconds(p), "{p}");
        }
    }

    #[test]
    fn format_wakes_on_the_minute_unless_it_shows_seconds() {
        let rt = Runtime::new();
        let utc = Zone::Fixed(FixedOffset::east_opt(0).unwrap());
        // 2026-10-05 09:41:07 UTC.
        let t0 = 1_791_193_267;
        let clock = std::rc::Rc::new(Clock::new(&rt, &types(), utc, at(t0)));
        assert_eq!(clock.next_wake(&rt), None, "nothing reads the clock");
        let c = clock.clone();
        let hm = rt.memo(move |rt| c.format(rt, "%H:%M"));
        let _w = rt.watch(hm.id()).unwrap();
        assert_eq!(hm.get(&rt).unwrap(), Value::text("09:41"));
        // Nothing reads seconds: wake at the next minute.
        assert_eq!(clock.next_wake(&rt), Some(at(t0 - 7 + 60)));
        clock.set_time(&rt, at(t0 + 10));
        let tick = rt.flush();
        assert!(tick.changed.is_empty(), "a second passing changes nothing");
        clock.set_time(&rt, at(t0 + 53));
        let tick = rt.flush();
        assert_eq!(tick.changed, vec![hm.id()]);
        assert_eq!(hm.get(&rt).unwrap(), Value::text("09:42"));
        let c = clock.clone();
        let hms = rt.memo(move |rt| c.format(rt, "%H:%M:%S"));
        let _w2 = rt.watch(hms.id()).unwrap();
        assert_eq!(clock.next_wake(&rt), Some(at(t0 + 54)));
        assert!(clock.format(&rt, "%Q").is_err());
    }

    #[test]
    fn calendar_has_six_weeks_from_monday() {
        let rt = Runtime::new();
        let t = types();
        let utc = Zone::Fixed(FixedOffset::east_opt(0).unwrap());
        let clock = Clock::new(&rt, &t, utc, at(1_791_193_267));
        let today = clock.read(&rt, "today").unwrap();
        assert_eq!(date_of(&t, &today), NaiveDate::from_ymd_opt(2026, 10, 5));
        let days = clock.days(&rt, &t, &today).unwrap();
        let days = days.as_list().unwrap();
        assert_eq!(days.len(), 42);
        // October 2026 starts on a Thursday: the grid starts Monday 28 Sep.
        let first = days[0].field(&t, "date").unwrap();
        assert_eq!(date_of(&t, first), NaiveDate::from_ymd_opt(2026, 9, 28));
        assert_eq!(days[0].field(&t, "in_month"), Some(&Value::Bool(false)));
        let todays: Vec<_> = days
            .iter()
            .filter(|d| d.field(&t, "today") == Some(&Value::Bool(true)))
            .collect();
        assert_eq!(todays.len(), 1);
        let prev = date_method(&t, &today, "add", &[Value::int(0), Value::int(-1)]).unwrap();
        assert_eq!(date_of(&t, &prev), NaiveDate::from_ymd_opt(2026, 9, 5));
        let f = date_method(&t, &prev, "format", &[Value::text("%B %Y")]).unwrap();
        assert_eq!(f, Value::text("September 2026"));
    }

    #[test]
    fn huge_date_shifts_are_errors_not_panics() {
        let t = types();
        let utc = Zone::Fixed(FixedOffset::east_opt(0).unwrap());
        let rt = Runtime::new();
        let today = Clock::new(&rt, &t, utc, at(1_791_193_267))
            .read(&rt, "today")
            .unwrap();
        for args in [
            vec![Value::int(0), Value::int(0), Value::float(1e18)],
            vec![Value::int(0), Value::float(1e30), Value::int(0)],
            vec![Value::int(0), Value::float(4_294_967_296.0)],
            vec![Value::int(0), Value::float(-4_294_967_296.0)],
            vec![Value::float(1e30)],
            vec![Value::float(-1e30)],
        ] {
            let r = date_method(&t, &today, "add", &args);
            assert!(r.is_err(), "{args:?} gave {r:?}");
        }
    }
}
