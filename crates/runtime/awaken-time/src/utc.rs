//! The UTC calendar codec the wall-clock port is spelled in.
//!
//! One implementation, owned beside the [`WallClock`](crate::WallClock) port
//! itself, so the Work queue, the Environment registry and every other durable
//! record render the same reading identically. It replaced a per-context frozen
//! literal: a record now carries the instant its own store observed. It also
//! replaced the copies other contexts had grown (the Session application
//! contract's millisecond projection and four second-precision formatters);
//! [`UtcTime`](crate::UtcTime) renders and parses through these functions only.

// The existing UTC wire vocabulary has four-digit years. Raw durable clocks
// retain their wider SQL domain; only calendar projection saturates here.
pub const MAX_RFC3339_NANOS: i128 = 253_402_300_799_999_999_999;

pub fn bounded_rfc3339_nanos(value: i128) -> i128 {
    value.clamp(0, MAX_RFC3339_NANOS)
}

pub fn parse_rfc3339_utc_nanos(value: &str) -> Option<i128> {
    let body = value.strip_suffix('Z')?;
    let (whole, fraction) = match body.split_once('.') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (body, ""),
    };
    let bytes = whole.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = decimal(&bytes[0..4])? as i64;
    let month = decimal(&bytes[5..7])?;
    let day = decimal(&bytes[8..10])?;
    let hour = decimal(&bytes[11..13])?;
    let minute = decimal(&bytes[14..16])?;
    let second = decimal(&bytes[17..19])?;
    if year < 1970
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
        || fraction.len() > 9
    {
        return None;
    }
    let mut subsecond = 0_i128;
    for byte in fraction.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        subsecond = subsecond * 10 + i128::from(byte - b'0');
    }
    for _ in fraction.len()..9 {
        subsecond *= 10;
    }
    let days = days_from_civil(year, month, day);
    let seconds = i128::from(days) * 86_400
        + i128::from(hour) * 3_600
        + i128::from(minute) * 60
        + i128::from(second);
    Some(seconds * 1_000_000_000 + subsecond)
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(*byte - b'0'))
    })
}

pub const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn format_rfc3339_utc_nanos(total_nanos: i128) -> String {
    let total_nanos = bounded_rfc3339_nanos(total_nanos);
    let seconds = total_nanos / 1_000_000_000;
    let subsecond = total_nanos % 1_000_000_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = i64::try_from(seconds % 86_400).unwrap_or_default();
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day / 60) % 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{subsecond:09}Z")
}

pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = (month_prime + if month_prime < 10 { 3 } else { -9 }) as u32;
    year += i64::from(month <= 2);
    (year, month, day)
}

/// Render a clock reading as the RFC-3339 UTC instant a durable record carries.
///
/// Whole seconds keep their historical spelling and sub-second precision is
/// preserved, so a value written here parses back through
/// [`parse_rfc3339_utc_nanos`] without loss.
#[must_use]
pub fn epoch_millis_to_rfc3339(epoch_millis: u64) -> String {
    let nanos = i128::from(project_epoch_millis(epoch_millis.into())) * 1_000_000;
    let text = format_rfc3339_utc_nanos(nanos);
    let (whole, fraction) = text
        .strip_suffix('Z')
        .expect("UTC codec")
        .split_once('.')
        .expect("fraction codec");
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{whole}Z")
    } else {
        format!("{whole}.{fraction}Z")
    }
}

/// Millisecond value represented by the public UTC projection. Consumers that
/// filter projected timestamps use this same normalization as the formatter.
#[must_use]
pub fn project_epoch_millis(epoch_millis: u128) -> u64 {
    epoch_millis.min((MAX_RFC3339_NANOS / 1_000_000) as u128) as u64
}

/// Decode the same persisted UTC vocabulary; malformed/unknown times remain absent.
#[must_use]
pub fn rfc3339_to_epoch_millis(value: &str) -> Option<u64> {
    parse_rfc3339_utc_nanos(value)
        .filter(|nanos| *nanos >= 0)
        .and_then(|nanos| u64::try_from(nanos / 1_000_000).ok())
}

#[cfg(kani)]
/// @obligation session_time.utc_projection_domain_is_total_exact_and_idempotent
#[kani::proof]
fn public_millisecond_projection_is_exact_bounded_and_nonwrapping() {
    // P1 every u128 at/below the UTC maximum is preserved; P2 every larger
    // value saturates to that maximum before narrowing. No input assumptions.
    // This numeric range lemma complements the Memory caller's exact
    // quantization proof; formatter/parser correspondence has executable
    // round-trip and independent calendar-boundary tests below.
    let millis: u128 = kani::any();
    let projected = project_epoch_millis(millis);
    assert!(projected <= 253_402_300_799_999);
    if millis <= 253_402_300_799_999 {
        assert_eq!(u128::from(projected), millis);
    } else {
        assert_eq!(projected, 253_402_300_799_999);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        // D1 every legal millisecond round-trips; D2 every larger u64 saturates
        // to the last representable millisecond. A fixed regression below also
        // checks known dates independently of the round-trip oracle.
        #[test]
        fn timestamp_projection_round_trips_its_declared_domain(millis in proptest::prelude::any::<u64>()) {
            // The filter's shared numeric projection and the wire codec must
            // represent the same instant for legal and saturated input classes.
            let wire = epoch_millis_to_rfc3339(millis);
            proptest::prop_assert_eq!(project_epoch_millis(millis.into()), millis.min(253_402_300_799_999));
            proptest::prop_assert_eq!(rfc3339_to_epoch_millis(&wire), Some(project_epoch_millis(millis.into())));
        }
    }

    #[test]
    fn timestamp_input_and_calendar_boundary_decision_table() {
        // D3 invalid syntax/date -> absent; D4 calendar boundaries -> exact
        // independent golden value; D5 formatter output always remains in the
        // declared four-digit UTC vocabulary, including u64/SQL extremes.
        for invalid in [
            "2026-09-06T00:00:00.Z",
            "2026-09-06T00:00:00..1Z",
            "2026-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00.1234567890Z",
            "2026-01-01T00:00:00+00:00",
            "\u{ff12}\u{ff10}\u{ff12}\u{ff16}-01-01T00:00:00Z",
        ] {
            assert_eq!(rfc3339_to_epoch_millis(invalid), None, "D3 {invalid}");
        }
        for (millis, expected) in [
            (946_684_800_000, "2000-01-01T00:00:00Z"),
            (951_782_400_000, "2000-02-29T00:00:00Z"),
            (4_107_542_400_000, "2100-03-01T00:00:00Z"),
            (253_402_300_799_999, "9999-12-31T23:59:59.999Z"),
        ] {
            assert_eq!(epoch_millis_to_rfc3339(millis), expected, "D4");
            assert_eq!(rfc3339_to_epoch_millis(expected), Some(millis), "D4");
        }
        for millis in [253_402_300_800_000, i64::MAX as u64, u64::MAX] {
            assert_eq!(
                epoch_millis_to_rfc3339(millis),
                "9999-12-31T23:59:59.999Z",
                "D5"
            );
        }
    }

    #[test]
    fn epoch_milliseconds_have_one_canonical_projection() {
        // Cause/effect table: zero -> epoch; subsecond -> exact fraction;
        // leap-day boundary -> exact Gregorian date; fractional trailing zeros
        // normalize and round-trip; malformed/pre-epoch input -> no observation.
        for millis in [0, 1, 10, 100, 999, 1_800_000_000_123] {
            assert_eq!(
                rfc3339_to_epoch_millis(&epoch_millis_to_rfc3339(millis)),
                Some(millis)
            );
        }
        assert_eq!(epoch_millis_to_rfc3339(100), "1970-01-01T00:00:00.1Z");
        assert_eq!(rfc3339_to_epoch_millis("invalid"), None);
        assert_eq!(rfc3339_to_epoch_millis("1969-12-31T23:59:59Z"), None);
        assert_eq!(rfc3339_to_epoch_millis("1969-12-31T23:59:59.9995Z"), None);
        assert_eq!(epoch_millis_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_millis_to_rfc3339(999), "1970-01-01T00:00:00.999Z");
        assert_eq!(
            epoch_millis_to_rfc3339(1_709_164_800_000),
            "2024-02-29T00:00:00Z"
        );
    }
}
