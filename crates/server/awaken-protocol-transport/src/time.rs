/// Format an epoch-millisecond instant as canonical RFC 3339 UTC at second
/// precision. Protocol and HTTP adapters share this one projection instead of
/// carrying private calendar implementations.
#[must_use]
pub fn epoch_millis_to_rfc3339(epoch_millis: u64) -> String {
    let seconds = (epoch_millis / 1_000) as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        seconds_of_day / 3_600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// Howard Hinnant's public-domain civil calendar algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause/effect graph: epoch milliseconds are truncated to the advertised
    /// second precision and Gregorian boundaries remain exact.
    ///
    /// | Rule | Epoch cause | Effect |
    /// |---|---|---|
    /// | T1 | zero | Unix epoch |
    /// | T2 | sub-second remainder | same canonical second |
    /// | T3 | leap-day boundary | correct Gregorian date |
    #[test]
    fn epoch_milliseconds_have_one_canonical_projection() {
        assert_eq!(epoch_millis_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_millis_to_rfc3339(999), "1970-01-01T00:00:00Z");
        assert_eq!(
            epoch_millis_to_rfc3339(1_709_164_800_000),
            "2024-02-29T00:00:00Z"
        );
    }
}
