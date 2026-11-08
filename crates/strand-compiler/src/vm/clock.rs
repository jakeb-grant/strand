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
use chrono::{
    DateTime, Datelike, FixedOffset, Local, Months, NaiveDate, NaiveDateTime, Timelike, Utc,
};
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

impl FormatItems for NaiveDateTime {
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

    /// `clock.today` (a `Date` without a time of day) / `clock.now` (one
    /// with the hour and minute), tracked on the minute.
    pub fn read(&self, rt: &Runtime, field: &str) -> Result<Value, Error> {
        match field {
            "today" => {
                let m = self.minute.get(rt)?;
                let d = self
                    .today_at(m)
                    .ok_or_else(|| fail("the clock is out of range"))?;
                Ok(self.date_value(d))
            }
            "now" => {
                let m = self.minute.get(rt)?;
                let d = self
                    .datetime(m)
                    .ok_or_else(|| fail("the clock is out of range"))?;
                Ok(match self.date {
                    Some(r) => datetime_record(r, d.naive_local()),
                    None => Value::Null,
                })
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

/// A `Date` record without a time of day: year, month, day, weekday
/// (Monday 1 … Sunday 7), `hour` and `minute` null.
pub fn date_record(r: RecordId, d: NaiveDate) -> Value {
    Value::record(r, date_fields(d, None))
}

/// A `Date` record with its time of day (hour and minute; seconds are
/// dropped).
pub fn datetime_record(r: RecordId, d: NaiveDateTime) -> Value {
    Value::record(r, date_fields(d.date(), Some((d.hour(), d.minute()))))
}

/// Whether `r` is the schema's own `Date` record (not a config `type`
/// of that name).
pub fn is_date(types: &TypeTable, r: RecordId) -> bool {
    let def = types.record(r);
    def.name == "Date" && matches!(def.origin, crate::ty::Origin::Schema)
}

/// A date with its time of day, if any (hour, minute), as a document's
/// date text reads into a `Date` (decisions.md doc-formats): what
/// [`date_from_text`] and [`date_from_unix`] return.
pub type DateParts = (NaiveDate, Option<(u32, u32)>);

/// A `Date` record of `parts`.
pub fn date_value(r: RecordId, (d, time): DateParts) -> Value {
    Value::record(r, date_fields(d, time))
}

/// The wall time in `zone` of an instant, to the minute (seconds and
/// fractions dropped, not rounded). `None` when the wall time is past
/// what chrono holds (an instant on its first or last day moved by an
/// offset): chrono's own `naive_local` would panic there.
fn in_zone<Tz: chrono::TimeZone>(t: DateTime<Tz>, zone: Zone) -> Option<DateParts> {
    let utc = t.naive_utc();
    let offset = match zone {
        Zone::Fixed(off) => off,
        Zone::Local => {
            chrono::Offset::fix(&chrono::TimeZone::offset_from_utc_datetime(&Local, &utc))
        }
    };
    let wall = utc.checked_add_offset(offset)?;
    Some((wall.date(), Some((wall.hour(), wall.minute()))))
}

/// (doc-formats) Date text as a document writes it, read in `zone`;
/// the input is trimmed and the first form that parses wins:
///
/// 1. digit text (`[-]digits[.digits]`), by its count of integer
///    digits ([`digit_date`]): 8 are ISO basic `YYYYMMDD`, 12
///    `YYYYMMDDHHMM` and 14 `YYYYMMDDHHMMSS` (the wall time as written),
///    9 to 11 Unix seconds, 13 milliseconds, 16 microseconds, 19
///    nanoseconds; any other count is refused (`2026` is not 1970);
/// 2. RFC 3339 / ISO 8601 with `Z` or an offset, also without seconds,
///    with `+hhmm` or `+hh` (PostgreSQL), or in the basic form
///    `20261107T195510Z`;
/// 3. RFC 2822 (RSS), again without a leading weekday when chrono
///    refuses it (a full or a wrong weekday);
/// 4. a naive `YYYY-MM-DD[T ]HH:MM[:SS[.f]]`, or the basic
///    `YYYYMMDDTHHMM[SS]` (iCalendar's local and floating times): the
///    wall time as written;
/// 5. `YYYY-MM-DD`: the date alone, with no time of day.
///
/// A trailing ` UTC` (`date -u '+%F %T %Z'`, Go's RFC 1123) reads as
/// `Z`. An instant is moved to `zone` and cut to the minute; one whose
/// wall time is past chrono's range is `None`. Anything else (ctime,
/// `08:04 PM`) is `None`; nothing panics.
pub fn date_from_text(t: &str, zone: Zone) -> Option<DateParts> {
    let t = t.trim();
    if t.is_empty() || t.len() > 64 {
        return None;
    }
    if let Some(d) = digit_date(t, zone) {
        return d;
    }
    // ` UTC` reads as `Z`: the forms below then take it.
    let utc = ["UTC", "utc"]
        .iter()
        .find_map(|z| t.strip_suffix(z)?.strip_suffix(' '));
    let t: std::borrow::Cow<'_, str> = match utc {
        Some(rest) if rest.contains(',') || rest.contains(char::is_alphabetic) => {
            format!("{rest} +0000").into()
        }
        Some(rest) => format!("{}Z", rest.trim_end()).into(),
        None => t.into(),
    };
    let t = &*t;
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return in_zone(d, zone);
    }
    // A zone written `Z` reads as `+0000` for the fallbacks below.
    let zoned = match t.strip_suffix(['Z', 'z']) {
        Some(rest) => format!("{rest}+0000"),
        None => t.to_string(),
    };
    // `%#z`: an offset of `+hh`, `+hhmm` or `+hh:mm`.
    for f in [
        "%Y-%m-%dT%H:%M%#z",
        "%Y-%m-%d %H:%M%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y%m%dT%H%M%S%#z",
        "%Y%m%dT%H%M%#z",
    ] {
        if let Ok(d) = DateTime::parse_from_str(&zoned, f) {
            return in_zone(d, zone);
        }
    }
    if let Ok(d) = DateTime::parse_from_rfc2822(t) {
        return in_zone(d, zone);
    }
    // `Saturday, 07 Nov 2026 …`, `Mon, 08 Nov 2026 …` (a Sunday): the
    // date and time decide, not the weekday.
    if let Some((word, rest)) = t.split_once(',')
        && word.chars().all(|c| c.is_ascii_alphabetic())
        && let Ok(d) = DateTime::parse_from_rfc2822(rest.trim_start())
    {
        return in_zone(d, zone);
    }
    for f in [
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y%m%dT%H%M%S",
        "%Y%m%dT%H%M",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, f) {
            return Some((d.date(), Some((d.hour(), d.minute()))));
        }
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .map(|d| (d, None))
}

/// Digit text: `Some(parsed)` when it is all digits (see
/// [`date_from_text`]), `None` when it is something else to try. The
/// count of integer digits decides what it is, never its size alone, so
/// `date +%Y%m%d%H%M%S`, journald's microseconds and a nanosecond
/// clock are not read as milliseconds of another century.
fn digit_date(t: &str, zone: Zone) -> Option<Option<DateParts>> {
    let negative = t.starts_with('-');
    let body = t.strip_prefix('-').unwrap_or(t);
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || !digits(int) || !digits(frac) {
        return None;
    }
    let calendar = !negative && frac.is_empty();
    let wall = |f: &str| {
        NaiveDateTime::parse_from_str(int, f)
            .ok()
            .map(|d| (d.date(), Some((d.hour(), d.minute()))))
    };
    let unix = |per_second: f64| {
        t.parse::<f64>()
            .ok()
            .and_then(|n| date_from_unix(n / per_second, zone))
    };
    Some(match int.len() {
        8 if calendar => NaiveDate::parse_from_str(int, "%Y%m%d")
            .ok()
            .map(|d| (d, None)),
        12 if calendar => wall("%Y%m%d%H%M"),
        14 if calendar => wall("%Y%m%d%H%M%S"),
        9..=11 => unix(1.0),
        13 => unix(1e3),
        16 => unix(1e6),
        19 => unix(1e9),
        _ => None,
    })
}

/// (doc-formats) A document's number as a date, read in `zone`: as its
/// digits written out are by [`date_from_text`], so a command printing
/// `20261107` (which a document reads as a JSON number) is 7 November
/// 2026 and `2026` or `0` is no date, as the same digits in quotes are.
pub fn date_from_number(n: f64, zone: Zone) -> Option<DateParts> {
    if !n.is_finite() {
        return None;
    }
    // f64's `Display` writes every digit, never an exponent.
    digit_date(&n.to_string(), zone).flatten()
}

/// Unix seconds, read in `zone`. Not a number, or out of chrono's
/// range, is `None`.
fn date_from_unix(secs: f64, zone: Zone) -> Option<DateParts> {
    if !secs.is_finite() {
        return None;
    }
    let secs = secs.floor();
    if secs.abs() > 1e15 {
        return None;
    }
    let utc = DateTime::<Utc>::from_timestamp(secs as i64, 0)?;
    in_zone(utc, zone)
}

/// A `Date`'s fields in the schema's order.
fn date_fields(d: NaiveDate, time: Option<(u32, u32)>) -> Vec<Value> {
    let (hour, minute) = match time {
        Some((h, m)) => (Value::int(h as i64), Value::int(m as i64)),
        None => (Value::Null, Value::Null),
    };
    vec![
        Value::int(d.year() as i64),
        Value::int(d.month() as i64),
        Value::int(d.day() as i64),
        Value::int(d.weekday().number_from_monday() as i64),
        hour,
        minute,
    ]
}

/// The date a `Date` record holds.
pub fn date_of(types: &TypeTable, v: &Value) -> Option<NaiveDate> {
    let n = |f: &str| v.field(types, f).and_then(Value::as_f64);
    NaiveDate::from_ymd_opt(n("year")? as i32, n("month")? as u32, n("day")? as u32)
}

/// The time of day a `Date` record holds, if it has one (`hour` set; a
/// null `minute` is 0). Out of range is none.
fn time_of(types: &TypeTable, v: &Value) -> Option<(u32, u32)> {
    let n = |f: &str| v.field(types, f).and_then(Value::as_f64);
    let hour = n("hour")?;
    let minute = n("minute").unwrap_or(0.0);
    ((0.0..24.0).contains(&hour) && (0.0..60.0).contains(&minute))
        .then_some((hour as u32, minute as u32))
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
    let time = time_of(types, recv);
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
                .map(|d| Value::record(r.ty, date_fields(d, time)))
                .ok_or_else(|| fail("date out of range"))
        }
        "format" => {
            let p = args.first().and_then(Value::as_text).unwrap_or("");
            // A time of day formats `%H:%M`; without one, a pattern
            // asking for it is an error, as before.
            match time.and_then(|(h, m)| d.and_hms_opt(h, m, 0)) {
                Some(dt) => Ok(Value::text(format_with(&dt, p)?)),
                None => Ok(Value::text(format_with(&d, p)?)),
            }
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

    /// (m6-services) `clock.now` and a notification's `time` carry the
    /// hour and minute; `clock.today` and a calendar's days do not, so
    /// `d.date == clock.today` still holds on today's cell.
    #[test]
    fn a_date_can_have_a_time_of_day() {
        let rt = Runtime::new();
        let t = types();
        let utc = Zone::Fixed(FixedOffset::east_opt(0).unwrap());
        // 2026-10-05 09:41:07 UTC.
        let clock = Clock::new(&rt, &t, utc, at(1_791_193_267));
        let now = clock.read(&rt, "now").unwrap();
        assert_eq!(now.field(&t, "hour"), Some(&Value::int(9)));
        assert_eq!(now.field(&t, "minute"), Some(&Value::int(41)));
        let hm = date_method(&t, &now, "format", &[Value::text("%H:%M")]).unwrap();
        assert_eq!(hm, Value::text("09:41"));
        // Moving keeps the time of day; the month's start is a date.
        let tomorrow = date_method(&t, &now, "add", &[Value::int(1)]).unwrap();
        assert_eq!(date_of(&t, &tomorrow), NaiveDate::from_ymd_opt(2026, 10, 6));
        assert_eq!(
            date_method(&t, &tomorrow, "format", &[Value::text("%a %H:%M")]).unwrap(),
            Value::text("Tue 09:41")
        );
        let start = date_method(&t, &now, "month_start", &[]).unwrap();
        assert_eq!(start.field(&t, "hour"), Some(&Value::Null));
        // Today has none, as the calendar's days have none.
        let today = clock.read(&rt, "today").unwrap();
        assert_eq!(today.field(&t, "hour"), Some(&Value::Null));
        assert_eq!(today.field(&t, "minute"), Some(&Value::Null));
        assert!(date_method(&t, &today, "format", &[Value::text("%H:%M")]).is_err());
        let days = clock.days(&rt, &t, &today).unwrap();
        let days = days.as_list().unwrap();
        assert!(
            days.iter().any(|d| d.field(&t, "date") == Some(&today)),
            "today's cell equals clock.today"
        );
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

    fn plus_one() -> Zone {
        Zone::Fixed(FixedOffset::east_opt(3600).unwrap())
    }

    fn ymd(y: i32, m: u32, d: u32, time: Option<(u32, u32)>) -> Option<DateParts> {
        Some((NaiveDate::from_ymd_opt(y, m, d).unwrap(), time))
    }

    /// (doc-formats) Every form the samples hold (edge/dates.txt), read
    /// under +01:00.
    #[test]
    fn dates_from_text_read_every_standard_form() {
        let cases = [
            // RFC 2822, as RSS writes it.
            (
                "Wed, 21 Oct 2026 19:29:36 +0000",
                ymd(2026, 10, 21, Some((20, 29))),
            ),
            (
                "Fri, 06 Nov 2026 18:19:19 -0000",
                ymd(2026, 11, 6, Some((19, 19))),
            ),
            (
                "Thu, 14 May 2026 19:25:26 GMT",
                ymd(2026, 5, 14, Some((20, 25))),
            ),
            (
                "Sat, 7 Nov 2026 09:05:00 GMT",
                ymd(2026, 11, 7, Some((10, 5))),
            ),
            (
                "Sat, 07 Nov 2026 09:05 EST",
                ymd(2026, 11, 7, Some((15, 5))),
            ),
            ("07 Nov 2026 09:05:00 +0100", ymd(2026, 11, 7, Some((9, 5)))),
            (
                "Saturday, 07 Nov 2026 09:05:00 +0000",
                ymd(2026, 11, 7, Some((10, 5))),
            ),
            // A wrong weekday: 8 Nov 2026 is a Sunday.
            (
                "Mon, 08 Nov 2026 09:05:00 +0000",
                ymd(2026, 11, 8, Some((10, 5))),
            ),
            // RFC 3339 / ISO 8601.
            ("2026-11-07T19:59:32Z", ymd(2026, 11, 7, Some((20, 59)))),
            (
                "2026-11-07T12:32:00-06:00",
                ymd(2026, 11, 7, Some((19, 32))),
            ),
            ("2026-11-07T20:13:03.040Z", ymd(2026, 11, 7, Some((21, 13)))),
            (
                "2026-10-24T06:49:28.599056Z",
                ymd(2026, 10, 24, Some((7, 49))),
            ),
            ("2026-11-07 19:59:32Z", ymd(2026, 11, 7, Some((20, 59)))),
            ("1970-01-01T00:00+00:00", ymd(1970, 1, 1, Some((1, 0)))),
            ("2026-11-07T20:37Z", ymd(2026, 11, 7, Some((21, 37)))),
            ("2026-11-07T20:37:15+0100", ymd(2026, 11, 7, Some((20, 37)))),
            // PostgreSQL's offset of hours alone.
            (
                "2026-11-07 21:30:00.123456+01",
                ymd(2026, 11, 7, Some((21, 30))),
            ),
            ("2026-11-07T21:30-05", ymd(2026, 11, 8, Some((3, 30)))),
            ("20261107T195510Z", ymd(2026, 11, 7, Some((20, 55)))),
            // A trailing ` UTC`: `date -u '+%F %T %Z'`, Go's RFC 1123.
            ("2026-11-07 19:55:10 UTC", ymd(2026, 11, 7, Some((20, 55)))),
            (
                "Sat, 07 Nov 2026 09:05:00 UTC",
                ymd(2026, 11, 7, Some((10, 5))),
            ),
            // Naive: the wall time as written, also in the basic form
            // iCalendar writes under a TZID.
            ("20261107T195510", ymd(2026, 11, 7, Some((19, 55)))),
            ("20261107T1955", ymd(2026, 11, 7, Some((19, 55)))),
            // Digit text by its count of digits: `date +%Y%m%d%H%M%S`,
            // the Wayback CDX timestamp, journald's microseconds.
            ("20261107195510", ymd(2026, 11, 7, Some((19, 55)))),
            ("202611071955", ymd(2026, 11, 7, Some((19, 55)))),
            ("1794081310891234", ymd(2026, 11, 7, Some((20, 55)))),
            ("2026-11-07T21:30", ymd(2026, 11, 7, Some((21, 30)))),
            ("2026-11-07 19:55:10", ymd(2026, 11, 7, Some((19, 55)))),
            // A date alone.
            ("2026-11-07", ymd(2026, 11, 7, None)),
            ("20261107", ymd(2026, 11, 7, None)),
            ("  2026-11-07\n", ymd(2026, 11, 7, None)),
            // Unix seconds and milliseconds.
            ("1794084315", ymd(2026, 11, 7, Some((21, 45)))),
            ("1794084315.9", ymd(2026, 11, 7, Some((21, 45)))),
            ("1794081310891", ymd(2026, 11, 7, Some((20, 55)))),
            ("-100000000", ymd(1966, 10, 31, Some((15, 13)))),
        ];
        for (text, want) in cases {
            assert_eq!(date_from_text(text, plus_one()), want, "{text:?}");
        }
    }

    #[test]
    fn dates_from_text_refuse_the_rest_and_never_panic() {
        for text in [
            "Sat Nov  7 20:45:15 2026",
            "08:04 PM",
            "05-AUG-10 16:58:37",
            "2026",
            "",
            "   ",
            "2026-13-01",
            "2026-02-30",
            "99999999999999999999",
            "-99999999999999999999.5",
            "iso8601",
            "20261307",
            "-20261107",
            "1.2.3",
            ".5",
            "-",
            "Mon,",
            "2026-11-07T25:00",
            "\u{0}\u{ffff}",
            "+999999-01-01T00:00:00Z",
        ] {
            assert_eq!(date_from_text(text, plus_one()), None, "{text:?}");
        }
        // Long input is refused without being looked at.
        assert_eq!(date_from_text(&"1".repeat(100_000), plus_one()), None);
        assert_eq!(date_from_text(&"Mon, ".repeat(10_000), Zone::Local), None);
    }

    #[test]
    fn date_from_unix_reads_seconds_and_refuses_the_impossible() {
        assert_eq!(
            date_from_unix(1_794_084_315.0, plus_one()),
            ymd(2026, 11, 7, Some((21, 45)))
        );
        assert_eq!(
            date_from_unix(0.0, plus_one()),
            ymd(1970, 1, 1, Some((1, 0)))
        );
        for n in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e300,
            -1e300,
            1e18,
        ] {
            assert_eq!(date_from_unix(n, plus_one()), None, "{n}");
        }
        // The local zone is read without panicking (UTC on wasm).
        assert!(date_from_unix(1_794_084_315.0, Zone::Local).is_some());
    }

    /// An instant on chrono's first or last day, moved by a zone's
    /// offset past it, is no date: chrono's `naive_local` panicked there.
    #[test]
    fn an_instant_at_the_edge_of_the_range_is_no_date_in_a_zone() {
        let minus = Zone::Fixed(FixedOffset::west_opt(7 * 3600).unwrap());
        for (text, zone) in [
            ("-262143-01-01T00:30Z", minus),
            ("+262142-12-31T23:30Z", plus_one()),
            ("-262143-01-01T00:30:00+00:00", minus),
        ] {
            assert_eq!(date_from_text(text, zone), None, "{text}");
        }
        // The same through Unix seconds: the first and last minutes
        // chrono holds. (No digit count of a document's number reaches
        // them any more: 11 digits of seconds end in the year 5138.)
        let first = NaiveDate::MIN.and_hms_opt(0, 30, 0).unwrap().and_utc();
        let last = NaiveDate::MAX.and_hms_opt(23, 30, 0).unwrap().and_utc();
        assert_eq!(date_from_unix(first.timestamp() as f64, minus), None);
        assert_eq!(date_from_unix(last.timestamp() as f64, plus_one()), None);
        for n in [-8_334_601_313_400_000.0, 8_210_266_875_000_000.0] {
            assert_ne!(
                date_from_number(n, minus).map(|d| d.0),
                Some(NaiveDate::MIN)
            );
            let _ = date_from_number(n, Zone::Local);
        }
        // Whatever the local zone is, nothing panics.
        for text in ["-262143-01-01T00:30Z", "+262142-12-31T23:30Z"] {
            let _ = date_from_text(text, Zone::Local);
        }
        let _ = date_from_unix(first.timestamp() as f64, Zone::Local);
        let _ = date_from_unix(last.timestamp() as f64, Zone::Local);
    }

    /// A number reads as its digits do: `20261107` printed by `date
    /// +%Y%m%d` is a JSON number to the document, and must not be 1970.
    #[test]
    fn a_number_reads_as_its_digits() {
        for (n, want) in [
            (20_261_107.0, ymd(2026, 11, 7, None)),
            (1_794_084_315.0, ymd(2026, 11, 7, Some((21, 45)))),
            (1_794_084_315.9, ymd(2026, 11, 7, Some((21, 45)))),
            (1_794_081_310_891.0, ymd(2026, 11, 7, Some((20, 55)))),
            (-100_000_000.0, ymd(1966, 10, 31, Some((15, 13)))),
            // Compact date-times are the wall time as written.
            (202_611_071_955.0, ymd(2026, 11, 7, Some((19, 55)))),
            (20_261_107_195_510.0, ymd(2026, 11, 7, Some((19, 55)))),
            // journald's microseconds, a nanosecond clock.
            (1_794_081_310_891_234.0, ymd(2026, 11, 7, Some((20, 55)))),
            (
                1_794_081_310_891_234_567.0,
                ymd(2026, 11, 7, Some((20, 55))),
            ),
        ] {
            assert_eq!(date_from_number(n, plus_one()), want, "{n}");
        }
        for n in [
            2026.0,
            0.0,
            12_345.6,
            20_261_307.0,
            -20_261_107.0,
            // 12 and 14 digits that are no date-time, and counts that are
            // no unit (15, 17, 18, 20 digits).
            202_613_071_955.0,
            20_261_107_256_510.0,
            179_408_131_089_123.0,
            17_940_813_108_912_345.0,
            179_408_131_089_123_456.0,
            17_940_813_108_912_345_678.0,
            f64::NAN,
            f64::INFINITY,
            1e300,
            -1e300,
            1e-300,
        ] {
            assert_eq!(date_from_number(n, plus_one()), None, "{n}");
        }
    }

    #[test]
    fn instants_move_to_the_zone_and_drop_seconds() {
        assert_eq!(
            date_from_text("2026-11-07T23:59:59Z", plus_one()),
            ymd(2026, 11, 8, Some((0, 59)))
        );
        let minus = Zone::Fixed(FixedOffset::west_opt(6 * 3600).unwrap());
        assert_eq!(
            date_from_text("2026-11-07T03:00:59.999Z", minus),
            ymd(2026, 11, 6, Some((21, 0)))
        );
        // The record built from it: the schema's `Date`, by field.
        let t = types();
        let r = t.find_schema_record("Date").unwrap();
        assert!(is_date(&t, r));
        let v = date_value(r, ymd(2026, 11, 7, None).unwrap());
        assert_eq!(v.field(&t, "weekday"), Some(&Value::int(6)));
        assert_eq!(v.field(&t, "hour"), Some(&Value::Null));
    }
}
