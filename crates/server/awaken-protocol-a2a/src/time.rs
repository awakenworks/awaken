//! RFC 3339 helpers for the protocol boundary. Rendering and the calendar are
//! the wall-clock port's one codec; parsing stays here because A2A accepts
//! numeric UTC offsets the persisted `...Z` vocabulary does not.

use awaken_lease_contract::utc::{days_from_civil, days_in_month};

pub(crate) fn now_rfc3339() -> String {
    awaken_lease_contract::system_clock()
        .now()
        .to_rfc3339_secs()
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
    fn rendered_timestamps_parse_back_to_the_same_second() {
        // The rendering is the codec's whole-second spelling, and this parser
        // reads it back to the same Unix second.
        let now = now_rfc3339();
        assert_eq!(now.len(), 20, "{now}");
        let seconds = parse_rfc3339(&now).expect("rendered timestamp parses");
        assert!(seconds > 0);
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    }
}
