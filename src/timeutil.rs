//! UTC calendar formatting from a Unix timestamp — hand-rolled because no
//! date/time crate is a direct dependency of this binary. Every *other*
//! UTC-timestamp render in this codebase goes through DuckDB's own
//! `::VARCHAR` cast on a live connection (watermarks, `ops`'s displayed
//! marks) rather than Rust-side calendar math; the two call sites that need
//! this module — a log filename's timestamp, and the published run
//! summary's `started_at`/`finished_at` — both exist before (or entirely
//! without) a DuckDB connection, so that idiom isn't available to them.

/// Days-since-Unix-epoch -> `(year, month, day)` in the proleptic Gregorian
/// calendar. Howard Hinnant's `civil_from_days` algorithm
/// (<https://howardhinnant.github.io/date_algorithms.html>, public domain),
/// transcribed to Rust; correctness is pinned by
/// `rfc3339_utc_matches_known_dates` below rather than re-derived here.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Render a Unix timestamp (seconds) as RFC 3339 UTC with no fractional
/// seconds: `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// The log-filename flavor of the same timestamp — `-` in place of `:` (the
/// ADR'd convention: `YYYY-MM-DDTHH-MM-SSZ`, no colons in a filename).
pub fn filename_utc(unix_secs: i64) -> String {
    rfc3339_utc(unix_secs).replace(':', "-")
}

/// The current wall-clock time as Unix seconds, saturating to 0 on a clock
/// before the epoch (defense in depth; never expected in practice).
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The inverse of `civil_from_days` (same source, same public-domain
/// algorithm): `(year, month, day)` -> days since the Unix epoch.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Parse the exact shape `rfc3339_utc` renders — `YYYY-MM-DDTHH:MM:SSZ`,
/// optionally with fractional seconds (ignored) — back to Unix seconds.
/// `None` for anything else: this is the round-trip for datamk's own
/// timestamps, not a general date parser.
pub fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    if !s.ends_with('Z') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Anything between the seconds and the trailing `Z` must be `.fraction`.
    let rest = &s[19..s.len() - 1];
    let fraction = rest.starts_with('.') && rest[1..].chars().all(|c| c.is_ascii_digit());
    if !(rest.is_empty() || fraction) {
        return None;
    }
    Some(days_from_civil(y, m as u32, d as u32) * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_utc_matches_known_dates() {
        // Cross-checked against Python's `datetime.fromtimestamp(secs, tz=utc)`.
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_751_500_680), "2025-07-02T23:58:00Z");
        assert_eq!(rfc3339_utc(1_800_000_000), "2027-01-15T08:00:00Z");
    }

    #[test]
    fn rfc3339_utc_handles_a_pre_epoch_timestamp() {
        assert_eq!(rfc3339_utc(-86_400), "1969-12-31T00:00:00Z");
    }

    #[test]
    fn filename_utc_replaces_colons_with_dashes() {
        assert_eq!(filename_utc(0), "1970-01-01T00-00-00Z");
    }

    #[test]
    fn rfc3339_round_trips() {
        for t in [0, 1_787_611_906, -86_400, 951_782_400] {
            assert_eq!(parse_rfc3339_utc(&rfc3339_utc(t)), Some(t));
        }
        assert_eq!(
            parse_rfc3339_utc("2026-08-24T22:51:46.123Z"),
            Some(1_787_611_906)
        );
        assert_eq!(parse_rfc3339_utc("2026-08-24 22:51:46"), None);
        assert_eq!(parse_rfc3339_utc("garbage"), None);
    }
}
