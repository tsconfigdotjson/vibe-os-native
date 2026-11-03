//! Timestamps: Unix seconds in the store, RFC 3339 in UTC on the socket.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, now.
pub fn now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// `secs` since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Seconds since the epoch of a UTC time, by its parts (month and day from
/// 1); `None` if a part is out of range.
pub fn unix_seconds(y: i64, m: u32, d: u32, h: u32, min: u32, s: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + i64::from(h * 3600 + min * 60 + s))
}

/// Seconds since the epoch of `YYYY-MM-DDTHH:MM:SSZ`, as [`rfc3339`]
/// writes it (fractions of a second are dropped); `None` for anything else.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let shaped = b.len() >= 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && s.ends_with('Z');
    if !shaped {
        return None;
    }
    let rest = &s[19..s.len() - 1];
    if !(rest.is_empty()
        || rest.len() > 1 && rest.starts_with('.') && rest[1..].bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let n = |a: usize, b: usize| -> Option<u32> {
        let p = s.get(a..b)?;
        p.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| p.parse().ok())?
    };
    unix_seconds(
        i64::from(n(0, 4)?),
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    )
}

/// The day count from 1970-01-01 of a proleptic Gregorian date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a day count from 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_793_361_600), "2026-10-30T12:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(4_107_542_399), "2100-02-28T23:59:59Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn parts_to_seconds_and_back() {
        assert_eq!(unix_seconds(1970, 1, 1, 0, 0, 0), Some(0));
        assert_eq!(unix_seconds(2026, 10, 30, 12, 0, 0), Some(1_793_361_600));
        assert_eq!(unix_seconds(2000, 2, 29, 0, 0, 0), Some(951_782_400));
        assert_eq!(unix_seconds(2100, 2, 28, 23, 59, 59), Some(4_107_542_399));
        assert_eq!(unix_seconds(1969, 12, 31, 23, 59, 59), Some(-1));
        assert_eq!(unix_seconds(2026, 13, 1, 0, 0, 0), None);
        assert_eq!(unix_seconds(2026, 1, 1, 24, 0, 0), None);
        for t in [0, 86_399, 1_793_361_600, 4_107_542_399, -86_401] {
            let s = rfc3339(t);
            let p = |a: usize, b: usize| s[a..b].parse::<u32>().unwrap();
            let y = s[0..4].parse::<i64>().unwrap();
            assert_eq!(
                unix_seconds(y, p(5, 7), p(8, 10), p(11, 13), p(14, 16), p(17, 19)),
                Some(t),
                "{s}"
            );
        }
    }

    #[test]
    fn parses_what_it_writes() {
        for t in [0, 1_793_361_600, 951_782_400, 4_107_542_399] {
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t));
        }
        assert_eq!(
            parse_rfc3339("2026-10-30T12:00:00.750Z"),
            Some(1_793_361_600)
        );
        for bad in [
            "",
            "2026-10-30",
            "2026-10-30T12:00:00",
            "2026-10-30T12:00:00+01:00",
            "2026-1x-30T12:00:00Z",
            "2026-13-30T12:00:00Z",
            "2026-10-30T12:00:00.Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn now_is_after_this_was_written() {
        assert!(now() > 1_793_361_600);
    }
}
