//! Small RFC 3339 helpers kept dependency-free for the protocol boundary.

use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs() as i64;
    format_epoch(seconds)
}

pub(crate) fn parse_rfc3339(value: &str) -> Option<i64> {
    let (date, clock_and_zone) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i64>().ok()?;
    let month = date_parts.next()?.parse::<u32>().ok()?;
    let day = date_parts.next()?.parse::<u32>().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = days_in_month(year, month);
    if !(1..=max_day).contains(&day) {
        return None;
    }

    let (clock, offset) = if let Some(clock) = clock_and_zone.strip_suffix('Z') {
        (clock, 0_i64)
    } else {
        let index = clock_and_zone.rfind(['+', '-'])?;
        let (clock, zone) = clock_and_zone.split_at(index);
        let sign = if zone.starts_with('-') { -1_i64 } else { 1_i64 };
        let (hours, minutes) = zone[1..].split_once(':')?;
        let hours = hours.parse::<i64>().ok()?;
        let minutes = minutes.parse::<i64>().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        (clock, sign * (hours * 3_600 + minutes * 60))
    };
    let mut clock_parts = clock.split(':');
    let hour = clock_parts.next()?.parse::<i64>().ok()?;
    let minute = clock_parts.next()?.parse::<i64>().ok()?;
    let second = clock_parts.next()?.split('.').next()?.parse::<i64>().ok()?;
    if clock_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

pub(crate) fn parse_optional_rfc3339(value: Option<&str>) -> Result<Option<i64>, String> {
    match value {
        Some(value) => parse_rfc3339(value)
            .map(Some)
            .ok_or_else(|| "invalid statusTimestampAfter; expected RFC 3339".to_string()),
        None => Ok(None),
    }
}

pub(crate) fn is_after(value: Option<&str>, after: Option<i64>) -> bool {
    after.is_none_or(|after| {
        value
            .and_then(parse_rfc3339)
            .is_some_and(|value| value > after)
    })
}

fn format_epoch(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let remaining = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remaining / 3_600,
        (remaining / 60) % 60,
        remaining % 60
    )
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

// Howard Hinnant's public-domain civil calendar algorithms.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let shifted_month = month as i64 + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offsets_as_the_same_instant() {
        assert_eq!(
            parse_rfc3339("2026-07-19T08:00:00+08:00"),
            parse_rfc3339("2026-07-19T00:00:00Z")
        );
    }

    #[test]
    fn epoch_format_is_canonical_utc() {
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
    }
}
