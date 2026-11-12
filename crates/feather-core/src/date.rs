//! Modification-date filters: `dm:today`, `dm:2026-11`, `dm:2025..2026-06`.
//!
//! A filter value names a period of local time, which is turned into a
//! half-open range of UTC FILETIMEs once, when the query is parsed. Matching
//! an entry is then two integer compares against its stored mtime.

/// Seconds from 1601-01-01 (the FILETIME epoch) to 1970-01-01.
const EPOCH_GAP: i64 = 11_644_473_600;
/// FILETIME ticks (100 ns) per second.
const TICKS: i64 = 10_000_000;
const DAY: i64 = 86_400;

/// How local time relates to UTC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Zone {
    /// A fixed offset in minutes east of UTC; tests use this.
    Fixed(i32),
    /// This computer's time zone, daylight saving time included.
    Local,
}

/// "Now" and the time zone that relative and calendar dates are read in.
/// Injected so tests are deterministic; [`Clock::system`] is the real one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Clock {
    /// The current time as a UTC FILETIME.
    pub now: u64,
    pub zone: Zone,
    /// The first day of a week: 0 is Monday, 6 is Sunday.
    pub week_start: u8,
}

impl Clock {
    /// The system clock, the local time zone, and the user's first day of
    /// the week.
    pub fn system() -> Self {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64);
        Clock {
            now: to_filetime(unix),
            zone: Zone::Local,
            week_start: system_week_start(),
        }
    }

    /// A fixed clock: `now` as a UTC FILETIME, local time `offset_minutes`
    /// east of UTC, weeks starting on Monday.
    pub fn fixed(now: u64, offset_minutes: i32) -> Self {
        Clock {
            now,
            zone: Zone::Fixed(offset_minutes),
            week_start: 0,
        }
    }

    fn now_unix(&self) -> i64 {
        (self.now / TICKS as u64) as i64 - EPOCH_GAP
    }

    /// Local wall-clock seconds (as if local time were UTC) for a UTC time.
    fn local_of(&self, utc: i64) -> i64 {
        match self.zone {
            Zone::Fixed(minutes) => utc + i64::from(minutes) * 60,
            Zone::Local => local::to_local(utc),
        }
    }

    /// The UTC time of a local wall-clock time.
    fn utc_of(&self, local: i64) -> i64 {
        match self.zone {
            Zone::Fixed(minutes) => local - i64::from(minutes) * 60,
            Zone::Local => local::to_utc(local),
        }
    }

    /// Today as days since 1970-01-01, in local time.
    fn today(&self) -> i64 {
        self.local_of(self.now_unix()).div_euclid(DAY)
    }

    /// Local midnight starting `day` (days since 1970-01-01), in UTC.
    fn midnight(&self, day: i64) -> i64 {
        self.utc_of(day * DAY)
    }
}

/// A half-open range of FILETIMEs: `start` is included, `end` is not.
/// `0` and `u64::MAX` mean the range is open on that side.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DateRange {
    pub start: u64,
    pub end: u64,
}

impl DateRange {
    pub const ALL: DateRange = DateRange {
        start: 0,
        end: u64::MAX,
    };

    /// True when `mtime` is inside. An mtime of 0 means the date is unknown,
    /// and an unknown date matches no date filter.
    #[inline]
    pub fn contains(&self, mtime: u64) -> bool {
        mtime != 0 && mtime >= self.start && mtime < self.end
    }

    /// Both ranges at once: what ANDing two date filters means.
    pub fn intersect(self, other: DateRange) -> DateRange {
        DateRange {
            start: self.start.max(other.start),
            end: self.end.min(other.end),
        }
    }

    /// True when every time in `self` is also in `wider`.
    pub fn within(&self, wider: &DateRange) -> bool {
        self.start >= wider.start && self.end <= wider.end
    }
}

/// A period in UTC seconds since 1970; `None` is open on that side.
#[derive(Clone, Copy, Debug)]
struct Period {
    start: Option<i64>,
    end: Option<i64>,
}

/// Parses the value after `dm:` or `date:`. `None` when it isn't a date.
pub fn parse_date_filter(value: &str, clock: &Clock) -> Option<DateRange> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    let (start, end) = if let Some((left, right)) = value.split_once("..") {
        let (left, right) = (left.trim(), right.trim());
        if left.is_empty() && right.is_empty() {
            return None;
        }
        let start = if left.is_empty() {
            None
        } else {
            Some(period(left, clock)?.start?)
        };
        let end = if right.is_empty() {
            None
        } else {
            // A range ending in an open period, like `..last7days`, would
            // reach to the end of time: that is never what was meant.
            Some(period(right, clock)?.end?)
        };
        (start, end)
    } else if let Some(rest) = value.strip_prefix(">=") {
        (Some(period(rest, clock)?.start?), None)
    } else if let Some(rest) = value.strip_prefix("<=") {
        (None, Some(period(rest, clock)?.end?))
    } else if let Some(rest) = value.strip_prefix('>') {
        // After the period: from its end on.
        (Some(period(rest, clock)?.end?), None)
    } else if let Some(rest) = value.strip_prefix('<') {
        // Before the period starts.
        (None, Some(period(rest, clock)?.start?))
    } else {
        let rest = value.strip_prefix('=').unwrap_or(&value);
        let period = period(rest, clock)?;
        (period.start, period.end)
    };
    Some(DateRange {
        start: start.map_or(0, to_filetime),
        end: end.map_or(u64::MAX, to_filetime),
    })
}

/// One named period: a keyword, a relative span, or a calendar date.
fn period(value: &str, clock: &Clock) -> Option<Period> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let year_only = value.len() == 4 && value.bytes().all(|b| b.is_ascii_digit());
    if year_only || (value.starts_with(|c: char| c.is_ascii_digit()) && value.contains(['-', '/']))
    {
        return calendar(value, clock);
    }
    let today = clock.today();
    let days = |first: i64, after: i64| Period {
        start: Some(clock.midnight(first)),
        end: Some(clock.midnight(after)),
    };
    let keyword: String = value.chars().filter(|c| *c != '_' && *c != '-').collect();
    match keyword.as_str() {
        "today" => return Some(days(today, today + 1)),
        "yesterday" => return Some(days(today - 1, today)),
        "thisweek" | "lastweek" => {
            let weekday = (today + 3).rem_euclid(7); // 1970-01-01 was a Thursday.
            let first = today - (weekday - i64::from(clock.week_start)).rem_euclid(7);
            return Some(if keyword == "thisweek" {
                days(first, first + 7)
            } else {
                days(first - 7, first)
            });
        }
        "thismonth" | "lastmonth" | "thisyear" | "lastyear" => {
            let (year, month, _) = civil_from_days(today);
            let (first, after) = match keyword.as_str() {
                "thismonth" => (days_from_civil(year, month, 1), month_after(year, month)),
                "lastmonth" => {
                    let (y, m) = if month == 1 {
                        (year - 1, 12)
                    } else {
                        (year, month - 1)
                    };
                    (days_from_civil(y, m, 1), days_from_civil(year, month, 1))
                }
                "thisyear" => (days_from_civil(year, 1, 1), days_from_civil(year + 1, 1, 1)),
                _ => (days_from_civil(year - 1, 1, 1), days_from_civil(year, 1, 1)),
            };
            return Some(days(first, after));
        }
        _ => {}
    }
    relative(&keyword, clock)
}

/// `last7days`, `30days`, `2hours`, `last2weeks`, `15min`. Hours and
/// minutes count back from now; days and weeks are whole calendar days,
/// today included, so `last1days` is today. All of them run on into the
/// future, so a file dated a little ahead still counts as recent.
fn relative(value: &str, clock: &Clock) -> Option<Period> {
    let value = value.strip_prefix("last").unwrap_or(value).trim();
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let count: i64 = value[..split].parse().ok()?;
    if !(1..=1_000_000).contains(&count) {
        return None;
    }
    let start = match value[split..].trim() {
        "min" | "mins" | "minute" | "minutes" => clock.now_unix() - count * 60,
        "h" | "hour" | "hours" => clock.now_unix() - count * 3600,
        "d" | "day" | "days" => clock.midnight(clock.today() - (count - 1)),
        "w" | "week" | "weeks" => clock.midnight(clock.today() - (count * 7 - 1)),
        _ => return None,
    };
    Some(Period {
        start: Some(start),
        end: None,
    })
}

/// `2026`, `2026-11`, `2026-11-11`; `/` works in place of `-`.
fn calendar(value: &str, clock: &Clock) -> Option<Period> {
    let parts: Vec<&str> = value.split(['-', '/']).collect();
    let number = |text: &str, max_len: usize| -> Option<i64> {
        if text.is_empty() || text.len() > max_len || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    // At least 1601, so always four digits.
    let year = number(parts[0], 4).filter(|year| (1601..=9999).contains(year))?;
    let (first, after) = match parts.len() {
        1 => (days_from_civil(year, 1, 1), days_from_civil(year + 1, 1, 1)),
        2 => {
            let month = number(parts[1], 2).filter(|month| (1..=12).contains(month))?;
            (days_from_civil(year, month, 1), month_after(year, month))
        }
        3 => {
            let month = number(parts[1], 2).filter(|month| (1..=12).contains(month))?;
            let day = number(parts[2], 2).filter(|day| *day >= 1)?;
            if day > days_in_month(year, month) {
                return None;
            }
            let first = days_from_civil(year, month, day);
            (first, first + 1)
        }
        _ => return None,
    };
    Some(Period {
        start: Some(clock.midnight(first)),
        end: Some(clock.midnight(after)),
    })
}

fn month_after(year: i64, month: i64) -> i64 {
    if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    month_after(year, month) - days_from_civil(year, month, 1)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day counted from 1970-01-01.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// A UTC FILETIME for seconds since 1970, clamped to the FILETIME range.
pub fn to_filetime(unix: i64) -> u64 {
    let seconds = unix.saturating_add(EPOCH_GAP).max(0) as u64;
    seconds.saturating_mul(TICKS as u64)
}

/// A UTC FILETIME for a UTC date and time; used by tests and fixtures.
pub fn filetime_utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> u64 {
    to_filetime(days_from_civil(year, month, day) * DAY + hour * 3600 + minute * 60)
}

#[cfg(windows)]
fn system_week_start() -> u8 {
    use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_IFIRSTDAYOFWEEK};
    let mut buffer = [0u16; 4];
    // A null locale name is the user's default locale. The value is a digit:
    // 0 for Monday through 6 for Sunday.
    let written = unsafe {
        GetLocaleInfoEx(
            std::ptr::null(),
            LOCALE_IFIRSTDAYOFWEEK,
            buffer.as_mut_ptr(),
            buffer.len() as i32,
        )
    };
    if written >= 2 && (u16::from(b'0')..=u16::from(b'6')).contains(&buffer[0]) {
        (buffer[0] - u16::from(b'0')) as u8
    } else {
        0
    }
}

#[cfg(not(windows))]
fn system_week_start() -> u8 {
    0
}

#[cfg(windows)]
mod local {
    use super::{civil_from_days, days_from_civil, DAY};
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::{
        SystemTimeToTzSpecificLocalTime, TzSpecificLocalTimeToSystemTime,
    };

    fn system_time(seconds: i64) -> SYSTEMTIME {
        let days = seconds.div_euclid(DAY);
        let rest = seconds.rem_euclid(DAY);
        let (year, month, day) = civil_from_days(days);
        SYSTEMTIME {
            wYear: year.clamp(1601, 30827) as u16,
            wMonth: month as u16,
            wDayOfWeek: 0,
            wDay: day as u16,
            wHour: (rest / 3600) as u16,
            wMinute: (rest / 60 % 60) as u16,
            wSecond: (rest % 60) as u16,
            wMilliseconds: 0,
        }
    }

    fn seconds(time: &SYSTEMTIME) -> i64 {
        days_from_civil(
            i64::from(time.wYear),
            i64::from(time.wMonth),
            i64::from(time.wDay),
        ) * DAY
            + i64::from(time.wHour) * 3600
            + i64::from(time.wMinute) * 60
            + i64::from(time.wSecond)
    }

    /// Converts with the rules in force on that date, so a summer date is
    /// read in summer time even in winter.
    pub fn to_local(utc: i64) -> i64 {
        let input = system_time(utc);
        let mut output: SYSTEMTIME = unsafe { std::mem::zeroed() };
        if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &input, &mut output) } == 0 {
            return utc;
        }
        seconds(&output)
    }

    pub fn to_utc(local: i64) -> i64 {
        let input = system_time(local);
        let mut output: SYSTEMTIME = unsafe { std::mem::zeroed() };
        if unsafe { TzSpecificLocalTimeToSystemTime(std::ptr::null(), &input, &mut output) } == 0 {
            return local;
        }
        seconds(&output)
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod local {
    use super::{civil_from_days, DAY};
    use std::os::raw::{c_char, c_int};

    /// `struct tm` as glibc and musl lay it out on 64-bit targets, where
    /// `long` and `time_t` are 64 bits.
    #[repr(C)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
        tm_gmtoff: i64,
        tm_zone: *const c_char,
    }

    extern "C" {
        fn localtime_r(time: *const i64, out: *mut Tm) -> *mut Tm;
        fn mktime(tm: *mut Tm) -> i64;
    }

    fn empty() -> Tm {
        Tm {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
            tm_gmtoff: 0,
            tm_zone: std::ptr::null(),
        }
    }

    pub fn to_local(utc: i64) -> i64 {
        let time = utc;
        let mut tm = empty();
        if unsafe { localtime_r(&time, &mut tm) }.is_null() {
            return utc;
        }
        utc + tm.tm_gmtoff
    }

    pub fn to_utc(local: i64) -> i64 {
        let days = local.div_euclid(DAY);
        let rest = local.rem_euclid(DAY);
        let (year, month, day) = civil_from_days(days);
        let mut tm = empty();
        tm.tm_year = (year - 1900) as c_int;
        tm.tm_mon = (month - 1) as c_int;
        tm.tm_mday = day as c_int;
        tm.tm_hour = (rest / 3600) as c_int;
        tm.tm_min = (rest / 60 % 60) as c_int;
        tm.tm_sec = (rest % 60) as c_int;
        // Let the C library work out whether summer time applies.
        tm.tm_isdst = -1;
        let utc = unsafe { mktime(&mut tm) };
        if utc == -1 {
            local
        } else {
            utc
        }
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_pointer_width = "64"))))]
mod local {
    pub fn to_local(utc: i64) -> i64 {
        utc
    }

    pub fn to_utc(local: i64) -> i64 {
        local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-11 (a Wednesday) 14:30 local, two hours east of UTC.
    fn clock() -> Clock {
        Clock::fixed(filetime_utc(2026, 11, 11, 12, 30), 120)
    }

    /// Local midnight of a date in the test clock's zone, as a FILETIME.
    fn local(year: i64, month: i64, day: i64) -> u64 {
        filetime_utc(year, month, day, 0, 0) - 2 * 3600 * TICKS as u64
    }

    fn range(value: &str) -> DateRange {
        parse_date_filter(value, &clock()).unwrap_or_else(|| panic!("{value} should parse"))
    }

    fn between(start: u64, end: u64) -> DateRange {
        DateRange { start, end }
    }

    #[test]
    fn civil_dates_round_trip() {
        for days in [-719_468, -1, 0, 1, 10_957, 20_768, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(filetime_utc(1601, 1, 1, 0, 0), 0);
    }

    #[test]
    fn keywords_are_calendar_periods_in_local_time() {
        assert_eq!(
            range("today"),
            between(local(2026, 11, 11), local(2026, 11, 12))
        );
        assert_eq!(
            range("yesterday"),
            between(local(2026, 11, 10), local(2026, 11, 11))
        );
        // Weeks start on Monday 2026-11-09 by default.
        assert_eq!(
            range("thisweek"),
            between(local(2026, 11, 9), local(2026, 11, 16))
        );
        assert_eq!(
            range("lastweek"),
            between(local(2026, 11, 2), local(2026, 11, 9))
        );
        assert_eq!(
            range("thismonth"),
            between(local(2026, 11, 1), local(2026, 12, 1))
        );
        assert_eq!(
            range("lastmonth"),
            between(local(2026, 10, 1), local(2026, 11, 1))
        );
        assert_eq!(
            range("thisyear"),
            between(local(2026, 1, 1), local(2027, 1, 1))
        );
        assert_eq!(
            range("lastyear"),
            between(local(2025, 1, 1), local(2026, 1, 1))
        );
        assert_eq!(range("this_week"), range("thisweek"));
        assert_eq!(range("TODAY"), range("today"));
    }

    #[test]
    fn weeks_follow_the_first_day_of_the_week() {
        let sunday = Clock {
            week_start: 6,
            ..clock()
        };
        assert_eq!(
            parse_date_filter("thisweek", &sunday),
            Some(between(local(2026, 11, 8), local(2026, 11, 15)))
        );
    }

    #[test]
    fn last_month_and_year_wrap_around_january() {
        let january = Clock::fixed(filetime_utc(2026, 1, 15, 12, 0), 0);
        assert_eq!(
            parse_date_filter("lastmonth", &january),
            Some(between(
                filetime_utc(2025, 12, 1, 0, 0),
                filetime_utc(2026, 1, 1, 0, 0)
            ))
        );
        let december = Clock::fixed(filetime_utc(2026, 12, 15, 12, 0), 0);
        assert_eq!(
            parse_date_filter("thismonth", &december),
            Some(between(
                filetime_utc(2026, 12, 1, 0, 0),
                filetime_utc(2027, 1, 1, 0, 0)
            ))
        );
    }

    #[test]
    fn local_today_differs_from_utc_today_near_midnight() {
        // 23:30 UTC on the 11th is already the 12th at UTC+2.
        let late = Clock::fixed(filetime_utc(2026, 11, 11, 23, 30), 120);
        assert_eq!(
            parse_date_filter("today", &late),
            Some(between(local(2026, 11, 12), local(2026, 11, 13)))
        );
        // And still the 11th at UTC-5.
        let west = Clock::fixed(filetime_utc(2026, 11, 12, 3, 0), -300);
        assert_eq!(
            parse_date_filter("today", &west),
            Some(between(
                filetime_utc(2026, 11, 11, 5, 0),
                filetime_utc(2026, 11, 12, 5, 0)
            ))
        );
    }

    #[test]
    fn relative_spans_count_back_from_now() {
        assert_eq!(range("last7days"), between(local(2026, 11, 5), u64::MAX));
        assert_eq!(range("7days"), range("last7days"));
        assert_eq!(range("last30days"), between(local(2026, 10, 13), u64::MAX));
        assert_eq!(range("last1days"), between(local(2026, 11, 11), u64::MAX));
        assert_eq!(range("last2weeks"), between(local(2026, 10, 29), u64::MAX));
        assert_eq!(
            range("last3hours"),
            between(filetime_utc(2026, 11, 11, 9, 30), u64::MAX)
        );
        assert_eq!(range("3h"), range("last3hours"));
        assert_eq!(
            range("15min"),
            between(filetime_utc(2026, 11, 11, 12, 15), u64::MAX)
        );
    }

    #[test]
    fn calendar_dates_cover_their_whole_period() {
        assert_eq!(range("2026"), between(local(2026, 1, 1), local(2027, 1, 1)));
        assert_eq!(
            range("2026-11"),
            between(local(2026, 11, 1), local(2026, 12, 1))
        );
        assert_eq!(
            range("2026-12"),
            between(local(2026, 12, 1), local(2027, 1, 1))
        );
        assert_eq!(
            range("2026-11-11"),
            between(local(2026, 11, 11), local(2026, 11, 12))
        );
        assert_eq!(range("2026/11/11"), range("2026-11-11"));
        assert_eq!(range("2026-1-5"), range("2026-01-05"));
        assert_eq!(
            range("2024-02-29"),
            between(local(2024, 2, 29), local(2024, 3, 1))
        );
        assert_eq!(range("=2026"), range("2026"));
    }

    #[test]
    fn ranges_and_comparisons() {
        assert_eq!(
            range("2026-01-01..2026-03-31"),
            between(local(2026, 1, 1), local(2026, 4, 1))
        );
        assert_eq!(
            range("2025..2026"),
            between(local(2025, 1, 1), local(2027, 1, 1))
        );
        assert_eq!(range("..2026-06"), between(0, local(2026, 7, 1)));
        assert_eq!(range("2026-06.."), between(local(2026, 6, 1), u64::MAX));
        assert_eq!(range(">2026-01-01"), between(local(2026, 1, 2), u64::MAX));
        assert_eq!(range(">=2026-01-01"), between(local(2026, 1, 1), u64::MAX));
        assert_eq!(range("<2025"), between(0, local(2025, 1, 1)));
        assert_eq!(range("<=2025"), between(0, local(2026, 1, 1)));
        assert_eq!(range(">=lastweek"), between(local(2026, 11, 2), u64::MAX));
        assert_eq!(
            range("lastmonth..today"),
            between(local(2026, 10, 1), local(2026, 11, 12))
        );
        assert_eq!(range("last7days.."), range("last7days"));
    }

    #[test]
    fn invalid_values_are_rejected() {
        for value in [
            "",
            "..",
            "soon",
            "2026-13",
            "2026-02-30",
            "2026-11-11-1",
            "26",
            "20260",
            "202",
            "2026-",
            "-2026",
            "1600",
            "last0days",
            "lastdays",
            "7fortnights",
            ">",
            "<=",
            "..last7days",
            ">last7days",
            "2026-01-01..later",
            "x2026",
        ] {
            assert_eq!(parse_date_filter(value, &clock()), None, "{value}");
        }
    }

    #[test]
    fn bounds_are_start_inclusive_end_exclusive() {
        let today = range("today");
        assert!(today.contains(today.start));
        assert!(!today.contains(today.start - 1));
        assert!(today.contains(today.end - 1));
        assert!(!today.contains(today.end));
        // An unknown date never matches.
        assert!(!DateRange::ALL.contains(0));
        assert!(DateRange::ALL.contains(1));
        let before = range("<2025");
        assert!(!before.contains(0));
        assert!(before.contains(1));
    }

    #[test]
    fn intersecting_ranges_keeps_the_overlap() {
        let both = range("2026").intersect(range("<2026-07"));
        assert_eq!(both, between(local(2026, 1, 1), local(2026, 7, 1)));
        assert!(both.within(&range("2026")));
        assert!(!range("2026").within(&both));
        let none = range("2025").intersect(range("2026"));
        assert!(!none.contains(local(2025, 6, 1)));
        assert!(!none.contains(local(2026, 6, 1)));
    }

    #[test]
    fn system_clock_reads_now() {
        let clock = Clock::system();
        assert!(clock.now > filetime_utc(2020, 1, 1, 0, 0));
        assert!(clock.week_start <= 6);
        // Today in local time contains now.
        let today = parse_date_filter("today", &clock).unwrap();
        assert!(today.contains(clock.now), "{today:?} {}", clock.now);
        // Converting there and back lands where it started, away from
        // daylight-saving changes.
        let noon = days_from_civil(2026, 6, 15) * DAY + 12 * 3600;
        assert_eq!(clock.local_of(clock.utc_of(noon)), noon);
    }
}
