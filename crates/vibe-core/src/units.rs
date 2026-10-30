//! Sizes and durations as people write them in `host.toml`: `"4G"`, `"60s"`.
//! On the socket they're plain numbers: bytes and seconds.

pub const KIB: u64 = 1 << 10;
pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
pub const TIB: u64 = 1 << 40;

const SIZE_UNITS: [(char, u64); 4] = [('T', TIB), ('G', GIB), ('M', MIB), ('K', KIB)];

/// Parses a size: a whole number with an optional binary unit, `K`, `M`, `G`
/// or `T` (any case, optionally followed by `B` or `iB`). No unit means
/// bytes.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let bad = || format!("`{s}` isn't a size; write it like \"4G\", \"512M\" or a number of bytes");
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if digits == 0 {
        return Err(bad());
    }
    let n: u64 = t[..digits].parse().map_err(|_| bad())?;
    let unit = t[digits..].trim_start().to_ascii_uppercase();
    let unit = unit
        .strip_suffix("IB")
        .or_else(|| unit.strip_suffix('B'))
        .unwrap_or(&unit);
    let multiplier = match unit {
        "" => 1,
        u => {
            let mut chars = u.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return Err(bad());
            };
            SIZE_UNITS
                .iter()
                .find(|(name, _)| *name == c)
                .map(|(_, m)| *m)
                .ok_or_else(bad)?
        }
    };
    n.checked_mul(multiplier)
        .ok_or_else(|| format!("`{s}` is too large"))
}

/// Formats a size in the largest unit that holds it exactly, or `None` when
/// it isn't a whole number of KiB (write those as a plain number of bytes).
pub fn format_size(bytes: u64) -> Option<String> {
    if bytes == 0 {
        return Some("0".into());
    }
    SIZE_UNITS
        .iter()
        .find(|(_, m)| bytes.is_multiple_of(*m))
        .map(|(name, m)| format!("{}{name}", bytes / m))
}

/// Parses a duration: a whole number with an optional `s`, `m` or `h`. No
/// unit means seconds.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let bad =
        || format!("`{s}` isn't a duration; write it like \"60s\", \"5m\" or a number of seconds");
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    if digits == 0 {
        return Err(bad());
    }
    let n: u64 = t[..digits].parse().map_err(|_| bad())?;
    let multiplier = match t[digits..].trim_start() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(bad()),
    };
    n.checked_mul(multiplier)
        .ok_or_else(|| format!("`{s}` is too long"))
}

/// Formats a duration in seconds, as `"60s"`.
pub fn format_duration(secs: u64) -> String {
    format!("{secs}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        for (s, n) in [
            ("4G", 4 * GIB),
            ("4g", 4 * GIB),
            ("4GB", 4 * GIB),
            ("4GiB", 4 * GIB),
            (" 512 M ", 512 * MIB),
            ("1T", TIB),
            ("3k", 3 * KIB),
            ("123", 123),
            ("0", 0),
            ("7b", 7),
        ] {
            assert_eq!(parse_size(s), Ok(n), "{s}");
        }
        for s in [
            "",
            "G",
            "4X",
            "4GG",
            "-1G",
            "1.5G",
            "4 G B",
            "99999999999999999999",
        ] {
            assert!(parse_size(s).is_err(), "{s}");
        }
        assert!(parse_size("20000000T").unwrap_err().contains("too large"));
    }

    #[test]
    fn size_format_round_trips() {
        for (n, s) in [
            (4 * GIB, Some("4G")),
            (1536 * MIB, Some("1536M")),
            (TIB, Some("1T")),
            (KIB, Some("1K")),
            (0, Some("0")),
            (1000, None),
        ] {
            assert_eq!(format_size(n).as_deref(), s, "{n}");
            if let Some(s) = s {
                assert_eq!(parse_size(s), Ok(n));
            }
        }
    }

    #[test]
    fn durations() {
        for (s, n) in [("60s", 60), ("60", 60), ("5m", 300), ("1h", 3600), ("0", 0)] {
            assert_eq!(parse_duration(s), Ok(n), "{s}");
        }
        for s in ["", "s", "5d", "1.5m", "-1s", "60S"] {
            assert!(parse_duration(s).is_err(), "{s}");
        }
        assert_eq!(format_duration(60), "60s");
        assert_eq!(parse_duration(&format_duration(90)), Ok(90));
    }
}
