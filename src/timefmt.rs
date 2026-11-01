//! Small time helpers, so the binary needs no date library.

/// Parses an ISO-8601 timestamp such as `2026-10-01T17:30:00.472092+00:00` into Unix seconds.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let n = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-')
        || !matches!(b.get(10), Some(b'T' | b't' | b' '))
        || b.get(13) != Some(&b':') || b.get(16) != Some(&b':')
        || !(1..=12).contains(&mo) || !(0..24).contains(&h)
        || !(0..60).contains(&mi) || !(0..60).contains(&se)
    {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match mo { 2 => if leap { 29 } else { 28 }, 4 | 6 | 9 | 11 => 30, _ => 31 };
    if !(1..=days).contains(&d) { return None }

    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start { return None }
    }
    let offset = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(&sign) if sign == b'+' || sign == b'-' => {
            if i + 6 != b.len() || b.get(i + 3) != Some(&b':') { return None }
            let (hours, minutes) = (n(i + 1..i + 3)?, n(i + 4..i + 6)?);
            if !(0..24).contains(&hours) || !(0..60).contains(&minutes) { return None }
            let secs = hours * 3600 + minutes * 60;
            if sign == b'-' {
                -secs
            } else {
                secs
            }
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Short countdown as shown next to the bar: `2d`, `4h`, `35m`, or `--` once it has passed.
pub fn countdown(secs: i64) -> String {
    if secs <= 0 {
        "--".to_string()
    } else if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", (secs / 60).max(1))
    }
}

/// Countdown with two units, for the detail panel: `2 天 5 小时`, `3 小时 12 分`, `35 分钟`.
pub fn countdown_long(secs: i64) -> String {
    if secs <= 0 {
        "--".to_string()
    } else if secs >= 86_400 {
        format!("{} 天 {} 小时", secs / 86_400, secs % 86_400 / 3600)
    } else if secs >= 3600 {
        format!("{} 小时 {} 分", secs / 3600, secs % 3600 / 60)
    } else {
        format!("{} 分钟", (secs / 60).max(1))
    }
}

/// How long ago something happened: `刚刚`, `3 分钟前`, `2 小时前`, `4 天前`.
pub fn age(secs: i64) -> String {
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86_400)
    }
}

/// Wall-clock label for the moment `t` as seen from `now`, in the zone `utc_offset`
/// seconds east of UTC: `今天 01:30`, `明天 10:00`, `10月4日 18:14`.
pub fn clock(t: i64, now: i64, utc_offset: i64) -> String {
    let local = t + utc_offset;
    let day = local.div_euclid(86_400);
    let today = (now + utc_offset).div_euclid(86_400);
    let minutes = local.rem_euclid(86_400) / 60;
    let time = format!("{:02}:{:02}", minutes / 60, minutes % 60);
    match day - today {
        0 => format!("今天 {time}"),
        1 => format!("明天 {time}"),
        _ => {
            let (month, day_of_month) = month_day_from_days(day);
            format!("{month}月{day_of_month}日 {time}")
        }
    }
}

/// Like `clock`, but names the weekday for a moment later this week:
/// `今天 13:30`, `明天 10:00`, `星期五 10:00`, `11月9日 10:00`.
pub fn weekday_clock(t: i64, now: i64, utc_offset: i64) -> String {
    const NAMES: [&str; 7] = ["星期四", "星期五", "星期六", "星期日", "星期一", "星期二", "星期三"];
    let day = (t + utc_offset).div_euclid(86_400);
    match day - (now + utc_offset).div_euclid(86_400) {
        2..=6 => {
            let minutes = (t + utc_offset).rem_euclid(86_400) / 60;
            // 1 January 1970 was a Thursday.
            format!("{} {:02}:{:02}", NAMES[day.rem_euclid(7) as usize], minutes / 60, minutes % 60)
        }
        _ => clock(t, now, utc_offset),
    }
}

/// Month and day of month for a day count since 1970-01-01; the inverse of `days_from_civil`.
fn month_day_from_days(days: i64) -> (i64, i64) {
    let doe = (days + 719_468).rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CST: i64 = 8 * 3600;

    #[test]
    fn formats_long_countdown_and_age() {
        assert_eq!(countdown_long(0), "--");
        assert_eq!(countdown_long(59), "1 分钟");
        assert_eq!(countdown_long(3 * 3600 + 12 * 60 + 5), "3 小时 12 分");
        assert_eq!(countdown_long(2 * 86_400 + 5 * 3600 + 59), "2 天 5 小时");
        assert_eq!(age(12), "刚刚");
        assert_eq!(age(200), "3 分钟前");
        assert_eq!(age(2 * 3600 + 5), "2 小时前");
        assert_eq!(age(4 * 86_400 + 5), "4 天前");
    }

    #[test]
    fn clock_is_relative_to_the_local_day() {
        // 2026-10-01T17:30Z is 01:30 on 2 October in UTC+8.
        let reset = 1_790_875_800;
        assert_eq!(clock(reset, 1_790_860_000, CST), "明天 01:30"); // asked at 21:06 on 1 October
        assert_eq!(clock(reset, 1_790_871_000, CST), "今天 01:30"); // asked at 00:10 on 2 October
        assert_eq!(clock(1_791_108_875, 1_790_860_000, CST), "10月4日 18:14");
        assert_eq!(clock(reset, 1_790_860_000, 0), "今天 17:30");
    }

    #[test]
    fn weekday_clock_names_days_this_week() {
        let now = 1_790_860_000; // Thursday 1 October 2026, 21:06 in UTC+8
        assert_eq!(weekday_clock(1_790_875_800, now, CST), "明天 01:30");
        assert_eq!(weekday_clock(1_791_108_875, now, CST), "星期日 18:14"); // 4 October
        assert_eq!(weekday_clock(now + 6 * 86_400, now, CST), "星期三 21:06");
        assert_eq!(weekday_clock(now + 7 * 86_400, now, CST), "10月8日 21:06");
    }

    #[test]
    fn month_and_day_invert_days_from_civil() {
        for (y, m, d) in [(1970, 1, 1), (2024, 2, 29), (2026, 10, 4), (2026, 12, 31), (2027, 1, 1), (2028, 3, 1)] {
            assert_eq!(month_day_from_days(days_from_civil(y, m, d)), (m, d));
        }
    }

    #[test]
    fn parses_utc_and_offsets() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2000-01-01T00:00:00+00:00"), Some(946_684_800));
        assert_eq!(parse_iso8601("2000-01-01T08:00:00+08:00"), Some(946_684_800));
        assert_eq!(parse_iso8601("1999-12-31T19:00:00.5-05:00"), Some(946_684_800));
        assert_eq!(parse_iso8601("2026-10-01T17:30:00.472092+00:00"), Some(1_790_875_800));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_iso8601(""), None);
        assert_eq!(parse_iso8601("tomorrow"), None);
        assert_eq!(parse_iso8601("2026-10-01"), None);
        for bad in ["2026-02-29T00:00:00Z", "2026-13-01T00:00:00Z", "2026-00-01T00:00:00Z",
            "2026-01-00T00:00:00Z", "2026-01-01T24:00:00Z", "2026/01/01T00:00:00Z",
            "2026-01-01T00:00:00+08:99", "2026-01-01T00:00:00+08", "2026-01-01T00:00:00Zjunk",
            "2026-01-01T00:00:00.Z", "2026-01-01T00:00:00"] {
            assert_eq!(parse_iso8601(bad), None, "{bad}");
        }
        assert!(parse_iso8601("2024-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn formats_countdown() {
        assert_eq!(countdown(-5), "--");
        assert_eq!(countdown(20), "1m");
        assert_eq!(countdown(35 * 60), "35m");
        assert_eq!(countdown(4 * 3600 + 59 * 60), "4h");
        assert_eq!(countdown(2 * 86_400 + 5), "2d");
    }
}
