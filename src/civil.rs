//! UTC calendar fields of Unix seconds, without a date dependency.

/// `(year, month, day, hour, minute, second)` in UTC, from Howard Hinnant's days-to-civil
/// algorithm. Correct for negative and leap-day timestamps.
pub fn utc_civil(epoch: i64) -> (i64, i64, i64, i64, i64, i64) {
    let (days, seconds) = (epoch.div_euclid(86_400), epoch.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::utc_civil;

    #[test]
    fn utc_dates_cover_epoch_and_leap_days() {
        assert_eq!(utc_civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_civil(-1), (1969, 12, 31, 23, 59, 59));
        assert_eq!(utc_civil(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(utc_civil(1_709_164_800), (2024, 2, 29, 0, 0, 0));
        assert_eq!(utc_civil(1_780_000_000), (2026, 5, 28, 20, 26, 40));
    }
}
