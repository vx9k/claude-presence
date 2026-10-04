//! Tiny, allocation-free time helpers. We only need three things: "now" in
//! epoch milliseconds, the local UTC offset (to bucket activity into the
//! user's calendar days), and an RFC 3339 parser for transcript timestamps.
//! Pulling in chrono/jiff for that would cost more than the code below.

use std::time::{SystemTime, UNIX_EPOCH};

pub const MINUTE_MS: i64 = 60_000;
pub const DAY_SECS: i64 = 86_400;

#[inline]
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Local UTC offset in seconds (east of UTC positive) for the current moment.
#[cfg(unix)]
pub fn local_offset_secs() -> i64 {
    // SAFETY: localtime_r is the thread-safe variant; `tm` is fully written
    // before we read it, and a null return is handled.
    unsafe {
        let t: libc::time_t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

#[cfg(windows)]
pub fn local_offset_secs() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    // SAFETY: plain out-parameter call into kernel32.
    unsafe {
        let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let id = GetTimeZoneInformation(&mut tzi);
        // Bias is "UTC = local + bias" in minutes, so the offset is its negation.
        let mut bias = tzi.Bias as i64;
        if id == TIME_ZONE_ID_DAYLIGHT {
            bias += tzi.DaylightBias as i64;
        } else {
            bias += tzi.StandardBias as i64;
        }
        -bias * 60
    }
}

/// Days since the Unix epoch in the given fixed offset.
#[inline]
pub fn day_number(ms: i64, offset_secs: i64) -> i32 {
    (ms.div_euclid(1000) + offset_secs).div_euclid(DAY_SECS) as i32
}

/// Minute of the (offset-local) day, 0..1440.
#[inline]
pub fn minute_of_day(ms: i64, offset_secs: i64) -> u16 {
    ((ms.div_euclid(1000) + offset_secs).rem_euclid(DAY_SECS) / 60) as u16
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[inline]
fn digits(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let s = b.get(at..at + n)?;
    let mut v = 0u32;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u32;
    }
    Some(v)
}

/// Parse an RFC 3339 timestamp (`2026-10-04T05:25:00.123Z`, `+02:00` offsets
/// accepted) into epoch milliseconds. Returns `None` on anything malformed.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let year = digits(b, 0, 4)? as i64;
    let month = digits(b, 5, 2)?;
    let day = digits(b, 8, 2)?;
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let hour = digits(b, 11, 2)? as i64;
    let min = digits(b, 14, 2)? as i64;
    let sec = digits(b, 17, 2)? as i64;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            if i - start < 3 {
                millis = millis * 10 + (b[i] - b'0') as i64;
            }
            i += 1;
        }
        let n = i - start;
        if n == 0 {
            return None;
        }
        for _ in n..3 {
            millis *= 10;
        }
    }
    let (offset, end) = match b.get(i)? {
        b'Z' | b'z' => (0, i + 1),
        sign @ (b'+' | b'-') => {
            let oh = digits(b, i + 1, 2)? as i64;
            let om = digits(b, i + 4, 2)? as i64;
            if b[i + 3] != b':' || oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 3600 + om * 60;
            (if *sign == b'-' { -o } else { o }, i + 6)
        }
        _ => return None,
    };
    if end != b.len() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * DAY_SECS + hour * 3600 + min * 60 + sec - offset;
    Some(secs * 1000 + millis)
}

/// Human duration: `3h 12m`, `12m`, `45s`.
pub fn fmt_duration_ms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m) = (s / 3600, (s % 3600) / 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// Compact hours for lifetime stats: `412h`, `3.4h`, `25m`.
pub fn fmt_hours_ms(ms: i64) -> String {
    let mins = ms.max(0) / MINUTE_MS;
    if mins < 60 {
        format!("{mins}m")
    } else if mins < 600 {
        // Truncate like the whole-hour branch, so 9h59m reads 9.9h, not 10.0h.
        format!("{}.{}h", mins / 60, mins % 60 / 6)
    } else {
        format!("{}h", mins / 60)
    }
}

/// Compact counts: `999`, `12.3k`, `4.56M`, `1.20B`.
pub fn fmt_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_949 => format!("{:.1}k", n as f64 / 1e3),
        999_950..=999_994_999 => format!("{:.2}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00.123Z"), Some(1_791_091_500_123));
        assert_eq!(parse_rfc3339_ms("2026-10-04T07:25:00.123+02:00"), parse_rfc3339_ms("2026-10-04T05:25:00.123Z"));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00.123456Z"), Some(1_791_091_500_123));
        assert_eq!(parse_rfc3339_ms("garbage"), None);
        assert_eq!(parse_rfc3339_ms("2026-13-04T05:25:00Z"), None);
    }

    #[test]
    fn rfc3339_offsets_and_fractions() {
        let utc = parse_rfc3339_ms("2026-10-04T05:25:00Z").unwrap();
        assert_eq!(parse_rfc3339_ms("2026-10-03T23:55:00-05:30"), Some(utc));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00+00:00"), Some(utc));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00-00:00"), Some(utc));
        assert_eq!(parse_rfc3339_ms("2026-10-04t05:25:00z"), Some(utc));
        assert_eq!(parse_rfc3339_ms("2026-10-04 05:25:00Z"), Some(utc));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00.1Z"), Some(utc + 100));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00.12Z"), Some(utc + 120));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00.999999999Z"), Some(utc + 999));
        assert_eq!(parse_rfc3339_ms("2026-10-04T05:25:00.5+01:00"), Some(utc - 3_600_000 + 500));
        assert_eq!(parse_rfc3339_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(parse_rfc3339_ms("1969-12-31T23:59:59Z"), Some(-1000));
        assert_eq!(parse_rfc3339_ms("2026-12-31T23:59:60Z"), parse_rfc3339_ms("2027-01-01T00:00:00Z"));
    }

    #[test]
    fn rfc3339_rejects_malformed() {
        for s in [
            "",
            "2026-10-04",
            "2026-10-04T05:25:00",
            "2026-10-04T05:25:00.Z",
            "2026-10-04T05:25:00+02",
            "2026-10-04T05:25:00+0200",
            "2026-10-04T05:25:00+02-00",
            "2026-10-04T05:25:00+24:00",
            "2026-10-04T05:25:00+02:60",
            "2026-10-04T05:25:00 Z",
            "2026-10-04X05:25:00Z",
            "2026/10/04T05:25:00Z",
            "2026-00-04T05:25:00Z",
            "2026-10-00T05:25:00Z",
            "2026-10-32T05:25:00Z",
            "2026-10-04T24:00:00Z",
            "2026-10-04T05:60:00Z",
            "2026-10-04T05:25:61Z",
            "2026-1a-04T05:25:00Z",
            "-026-10-04T05:25:00Z",
            "２０２６-10-04T05:25:00Z",
            "2026-10-04T05:25:00.１Z",
            "2026-10-04T05:25:00Zjunk",
            "2026-10-04T05:25:00Z ",
            "2026-10-04T05:25:00+02:00x",
            "2026-10-04T05:25:00.5+02:00:00",
        ] {
            assert_eq!(parse_rfc3339_ms(s), None, "{s:?}");
        }
    }

    #[test]
    fn rfc3339_never_panics() {
        // Every prefix and every single-byte corruption of valid inputs,
        // including multi-byte characters landing on fixed offsets.
        let good = ["2026-10-04T05:25:00.123+02:00", "2026-10-04T05:25:00Z"];
        for g in good {
            for i in 0..=g.len() {
                let _ = parse_rfc3339_ms(&g[..i]);
                for rep in ["", "é", "\u{1F600}", "x", "9", "+", ".", ":"] {
                    let s = format!("{}{rep}{}", &g[..i], g.get(i + 1..).unwrap_or(""));
                    let _ = parse_rfc3339_ms(&s);
                }
            }
        }
    }

    #[test]
    fn day_math() {
        assert_eq!(day_number(0, 0), 0);
        assert_eq!(day_number(-1, 0), -1);
        assert_eq!(day_number(23 * 3_600_000, 3600), 1);
        assert_eq!(minute_of_day(90 * 60_000, 0), 90);
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(12_345), "12.3k");
        assert_eq!(fmt_count(999_960), "1.00M");
        assert_eq!(fmt_count(4_560_000), "4.56M");
        assert_eq!(fmt_hours_ms(25 * MINUTE_MS), "25m");
        assert_eq!(fmt_hours_ms(204 * MINUTE_MS), "3.4h");
        assert_eq!(fmt_hours_ms(1000 * MINUTE_MS), "16h");
        assert_eq!(fmt_duration_ms(3 * 3_600_000 + 12 * MINUTE_MS), "3h 12m");
    }

    #[test]
    fn formatting_edges() {
        assert_eq!(fmt_duration_ms(-5_000), "0s");
        assert_eq!(fmt_duration_ms(59_999), "59s");
        assert_eq!(fmt_duration_ms(60_000), "1m");
        assert_eq!(fmt_duration_ms(3_600_000), "1h 0m");
        let _ = fmt_duration_ms(i64::MAX);
        let _ = fmt_duration_ms(i64::MIN);

        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1.0k");
        assert_eq!(fmt_count(999_949), "999.9k");
        assert_eq!(fmt_count(999_950), "1.00M");
        assert_eq!(fmt_count(999_994_999), "999.99M");
        assert_eq!(fmt_count(999_995_000), "1.00B");
        let _ = fmt_count(u64::MAX);

        assert_eq!(fmt_hours_ms(-1), "0m");
        assert_eq!(fmt_hours_ms(59 * MINUTE_MS), "59m");
        assert_eq!(fmt_hours_ms(60 * MINUTE_MS), "1.0h");
        // 9h59m must not round up to "10.0h" (and then read "10h" a minute later).
        assert_eq!(fmt_hours_ms(599 * MINUTE_MS), "9.9h");
        assert_eq!(fmt_hours_ms(600 * MINUTE_MS), "10h");
        assert_eq!(fmt_hours_ms(659 * MINUTE_MS), "10h");
        let _ = fmt_hours_ms(i64::MAX);
    }
}
