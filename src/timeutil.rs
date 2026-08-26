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
}
