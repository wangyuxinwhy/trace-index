//! Strict byte-level RFC 3339 timestamp parsing to UTC epoch milliseconds.
//!
//! Shared across all Runtime adapters. Hand-rolled rather than pulling in a
//! full calendar/time-zone crate because Trace Index only converts RFC 3339
//! trace instants into UTC epoch milliseconds.

/// Parses an RFC 3339 timestamp (or bare UTC `YYYY-MM-DDTHH:MM:SS[.frac]`)
/// into Unix epoch milliseconds.
///
/// Rejects malformed delimiters, out-of-range calendar or clock components,
/// empty fractional parts, malformed offsets, and trailing bytes.
pub(crate) fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }

    let year = parse_4(bytes, 0)?;
    let month = parse_2(bytes, 5)?;
    let day = parse_2(bytes, 8)?;
    let hour = parse_2(bytes, 11)?;
    let minute = parse_2(bytes, 14)?;
    let raw_second = parse_2(bytes, 17)?;

    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || raw_second > 60 {
        return None;
    }
    // Unix epoch milliseconds do not represent leap seconds separately.
    let second = raw_second.min(59);

    let mut pos = 19;
    let millis = if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let frac_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        let frac_len = pos - frac_start;
        if frac_len == 0 {
            return None;
        }
        let d0 = i64::from(bytes[frac_start] - b'0');
        let d1 = if frac_len >= 2 {
            i64::from(bytes[frac_start + 1] - b'0')
        } else {
            0
        };
        let d2 = if frac_len >= 3 {
            i64::from(bytes[frac_start + 2] - b'0')
        } else {
            0
        };
        d0 * 100 + d1 * 10 + d2
    } else {
        0
    };

    let offset_seconds = match &bytes[pos..] {
        [] | [b'Z' | b'z'] => 0_i64,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let off_hours = parse_2(bytes, pos + 1)?;
            let off_minutes = parse_2(bytes, pos + 4)?;
            if off_hours > 23 || off_minutes > 59 {
                return None;
            }
            let magnitude = off_hours * 3_600 + off_minutes * 60;
            if *sign == b'-' { -magnitude } else { magnitude }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_sub(offset_seconds)?;
    secs.checked_mul(1_000)?.checked_add(millis)
}

fn parse_2(bytes: &[u8], offset: usize) -> Option<i64> {
    let d0 = ascii_digit(bytes[offset])?;
    let d1 = ascii_digit(bytes[offset + 1])?;
    Some(d0 * 10 + d1)
}

fn parse_4(bytes: &[u8], offset: usize) -> Option<i64> {
    let hi = parse_2(bytes, offset)?;
    let lo = parse_2(bytes, offset + 2)?;
    Some(hi * 100 + lo)
}

fn ascii_digit(byte: u8) -> Option<i64> {
    byte.is_ascii_digit().then(|| i64::from(byte - b'0'))
}

const fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since the Unix epoch, using Howard Hinnant's civil-date algorithm.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::parse_timestamp_ms;

    /// 2026-09-07T13:40:00Z as epoch milliseconds, computed independently of
    /// the parser under test: days since 1970-01-01 times 86 400 seconds.
    const SEPT_7_2026_13_40_UTC_MS: i64 = (20_703 * 86_400 + 13 * 3_600 + 40 * 60) * 1_000;

    #[test]
    fn timestamp_offsets_resolve_to_the_same_utc_instant() {
        assert_eq!(
            parse_timestamp_ms("2026-09-07T13:40:00Z"),
            Some(SEPT_7_2026_13_40_UTC_MS)
        );
        assert_eq!(
            parse_timestamp_ms("2026-09-07t13:40:00z"),
            Some(SEPT_7_2026_13_40_UTC_MS)
        );
        assert_eq!(
            parse_timestamp_ms("2026-09-07 13:40:00Z"),
            Some(SEPT_7_2026_13_40_UTC_MS)
        );
        assert_eq!(
            parse_timestamp_ms("2026-09-07T21:40:00+08:00"),
            Some(SEPT_7_2026_13_40_UTC_MS)
        );
        assert_eq!(
            parse_timestamp_ms("2026-09-07T08:10:00-05:30"),
            Some(SEPT_7_2026_13_40_UTC_MS)
        );
    }

    #[test]
    fn timestamp_offset_crossing_midnight_keeps_the_utc_day() {
        let sixteen_utc = parse_timestamp_ms("2026-09-07T16:00:00Z");
        assert_eq!(
            parse_timestamp_ms("2026-09-08T00:00:00+08:00"),
            sixteen_utc,
            "next-day midnight in UTC+8 is the same instant as 16:00Z"
        );
        assert_eq!(
            sixteen_utc,
            Some(SEPT_7_2026_13_40_UTC_MS + (2 * 3_600 + 20 * 60) * 1_000)
        );
    }

    #[test]
    fn timestamp_without_zone_is_read_as_utc() {
        assert_eq!(
            parse_timestamp_ms("2026-09-07T13:40:00"),
            parse_timestamp_ms("2026-09-07T13:40:00Z"),
            "a bare timestamp is not shifted by any host time zone"
        );
    }

    #[test]
    fn timestamp_fraction_is_truncated_to_milliseconds() {
        assert_eq!(
            parse_timestamp_ms("2026-09-07T13:40:00.5Z"),
            Some(SEPT_7_2026_13_40_UTC_MS + 500)
        );
        assert_eq!(
            parse_timestamp_ms("2026-09-07T13:40:00.05Z"),
            Some(SEPT_7_2026_13_40_UTC_MS + 50)
        );
        assert_eq!(
            parse_timestamp_ms("2026-09-07T13:40:00.123456+08:00"),
            Some(SEPT_7_2026_13_40_UTC_MS - 8 * 3_600 * 1_000 + 123)
        );
    }

    #[test]
    fn calendar_bounds_and_leap_years_are_validated() {
        assert!(parse_timestamp_ms("2024-02-29T00:00:00Z").is_some());
        assert!(parse_timestamp_ms("2000-02-29T00:00:00Z").is_some());
        assert_eq!(parse_timestamp_ms("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_timestamp_ms("1900-02-29T00:00:00Z"), None);
        assert_eq!(parse_timestamp_ms("2026-00-01T00:00:00Z"), None);
        assert_eq!(parse_timestamp_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_timestamp_ms("2026-04-31T00:00:00Z"), None);
        assert_eq!(parse_timestamp_ms("2026-09-07T24:00:00Z"), None);
        assert_eq!(parse_timestamp_ms("2026-09-07T13:60:00Z"), None);
        assert_eq!(parse_timestamp_ms("2026-09-07T13:40:61Z"), None);
    }

    #[test]
    fn malformed_timestamp_is_absent_not_zero() {
        for bad in [
            "",
            "2026-09-07",
            "2026/09/07T13:40:00Z",
            "2026-09-07X13:40:00Z",
            "2026-09-07T13-40-00Z",
            "+202-09-07T13:40:00Z",
            "2026-09-07T13:40:00.Z",
            "2026-09-07T13:40:00 UTC",
            "2026-09-07T13:40:00Zextra",
            "2026-09-07T13:40:00+xx:00",
            "2026-09-07T13:40:00+08:xx",
            "2026-09-07T13:40:00+24:00",
            "2026-09-07T13:40:00+08:60",
            "2026-09-07T13:40:00+08:00junk",
            "2026-09-07T13:40:00+08",
        ] {
            assert_eq!(parse_timestamp_ms(bad), None, "expected None for {bad:?}");
        }
    }
}
