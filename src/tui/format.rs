//! Short human-readable numbers, durations and dates for the dashboard.

use crate::timeutil::civil_from_days;

/// `999`, `1.2k`, `3.4M`, `5.6B`.
pub fn count(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "k")];
    for (div, unit) in UNITS {
        if n >= div {
            let tenths = n / (div / 10);
            return if tenths >= 1000 {
                format!("{}{unit}", tenths / 10)
            } else {
                format!("{}.{}{unit}", tenths / 10, tenths % 10)
            };
        }
    }
    n.to_string()
}

/// `45s`, `12m`, `3h 05m`, `2d 4h`; negative counts as zero.
pub fn duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h {:02}m", s / 3600, s / 60 % 60),
        _ => format!("{}d {}h", s / 86_400, s / 3600 % 24),
    }
}

/// `YYYY-MM-DD` of a day number (days since 1970-01-01).
pub fn date(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `MM-DD`, for chart axes.
pub fn short_date(day: i64) -> String {
    let (_, m, d) = civil_from_days(day);
    format!("{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1.0k");
        assert_eq!(count(1_250), "1.2k");
        assert_eq!(count(999_999), "999k");
        assert_eq!(count(3_400_000), "3.4M");
        assert_eq!(count(u64::MAX), "18446744073B");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(-5), "0s");
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(12 * 60_000 + 59_000), "12m");
        assert_eq!(duration(3 * 3_600_000 + 5 * 60_000), "3h 05m");
        assert_eq!(duration(2 * 86_400_000 + 4 * 3_600_000), "2d 4h");
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(20_000), "2024-10-04");
        assert_eq!(short_date(20_000), "10-04");
    }
}
