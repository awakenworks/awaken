//! One wall-clock reading and its conversions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::utc;

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// A wall-clock reading: the time elapsed since the Unix epoch.
///
/// Every conversion a durable owner needs is a method here, so a record's
/// epoch milliseconds, its Unix nanoseconds and its RFC 3339 spelling are
/// projections of one value and cannot disagree. Readings before the epoch do
/// not exist: they saturate to [`UtcTime::UNIX_EPOCH`] (see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct UtcTime(Duration);

impl UtcTime {
    /// 1970-01-01T00:00:00Z, and every pre-epoch reading.
    pub const UNIX_EPOCH: Self = Self(Duration::ZERO);

    /// A host clock reading. A host clock set before 1970 saturates to the
    /// epoch rather than failing: the reading is still the latest fact the host
    /// can give, and every consumer already orders the epoch first.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        Self(time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO))
    }

    #[must_use]
    pub const fn from_duration_since_epoch(elapsed: Duration) -> Self {
        Self(elapsed)
    }

    #[must_use]
    pub const fn duration_since_epoch(self) -> Duration {
        self.0
    }

    #[must_use]
    pub const fn from_epoch_ms(epoch_ms: u64) -> Self {
        Self(Duration::from_millis(epoch_ms))
    }

    /// A signed persisted millisecond; a negative value saturates to the epoch.
    #[must_use]
    pub fn from_epoch_ms_i64(epoch_ms: i64) -> Self {
        Self::from_epoch_ms(u64::try_from(epoch_ms).unwrap_or(0))
    }

    /// Epoch milliseconds in the signed persisted domain.
    ///
    /// Saturates at `i64::MAX`, so every reading fits the SQL integer column a
    /// store writes it to and never wraps negative.
    #[must_use]
    pub fn as_epoch_ms(self) -> u64 {
        self.as_epoch_ms_i64().unsigned_abs()
    }

    /// Epoch milliseconds as the signed persisted value, saturating at `i64::MAX`.
    #[must_use]
    pub fn as_epoch_ms_i64(self) -> i64 {
        i64::try_from(self.0.as_millis()).unwrap_or(i64::MAX)
    }

    #[must_use]
    pub fn from_unix_nanos(unix_nanos: u128) -> Self {
        let seconds = u64::try_from(unix_nanos / NANOS_PER_SECOND).unwrap_or(u64::MAX);
        let subsecond = (unix_nanos % NANOS_PER_SECOND) as u32;
        Self(Duration::new(seconds, subsecond))
    }

    #[must_use]
    pub const fn as_unix_nanos(self) -> u128 {
        self.0.as_nanos()
    }

    /// The RFC 3339 UTC spelling a durable record carries in an observation
    /// field, at millisecond precision. Whole seconds keep the historical
    /// `...:SSZ` spelling and trailing fractional zeros are dropped, so the
    /// value parses back through [`Self::parse_rfc3339`] without loss.
    #[must_use]
    pub fn to_rfc3339(self) -> String {
        utc::epoch_millis_to_rfc3339(self.as_epoch_ms())
    }

    /// The fixed-width `YYYY-MM-DDTHH:MM:SSZ` spelling, truncated to the whole
    /// second, for vocabularies whose lexical order must equal chronological
    /// order (IAM grants, A2A task status, webhook envelopes).
    #[must_use]
    pub fn to_rfc3339_secs(self) -> String {
        let whole_seconds = i128::from(self.0.as_secs().min(i64::MAX as u64) as i64);
        let text = utc::format_rfc3339_utc_nanos(whole_seconds * NANOS_PER_SECOND as i128);
        let (whole, _fraction) = text.split_once('.').expect("UTC codec renders a fraction");
        format!("{whole}Z")
    }

    /// Parse the codec's own `...Z` vocabulary (up to nanosecond precision).
    /// Malformed text and pre-epoch instants are absent, not saturated: an
    /// unreadable persisted time is not a reading.
    #[must_use]
    pub fn parse_rfc3339(value: &str) -> Option<Self> {
        utc::parse_rfc3339_utc_nanos(value)
            .and_then(|nanos| u128::try_from(nanos).ok())
            .map(Self::from_unix_nanos)
    }

    #[must_use]
    pub const fn saturating_add(self, elapsed: Duration) -> Self {
        Self(self.0.saturating_add(elapsed))
    }

    /// The time from `earlier` to `self`; zero when `earlier` is not earlier.
    #[must_use]
    pub const fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_share_one_reading_and_saturate_at_their_domain_edges() {
        // Cause/effect decision table.
        // C1 a pre-epoch host reading; C2 a negative persisted millisecond;
        // C3 a millisecond beyond i64; C4 a nanosecond reading with a
        // sub-millisecond part; C5 an ordinary millisecond.
        // E1 C1/C2 read as the epoch; E2 C3 persists as i64::MAX and never
        // wraps negative; E3 C4 keeps every nanosecond and truncates to its
        // millisecond; E4 C5 round-trips through every projection.
        let before = UNIX_EPOCH - Duration::from_secs(5);
        assert_eq!(
            UtcTime::from_system_time(before),
            UtcTime::UNIX_EPOCH,
            "E1 C1"
        );
        assert_eq!(UtcTime::from_epoch_ms_i64(-1), UtcTime::UNIX_EPOCH, "E1 C2");

        let far = UtcTime::from_epoch_ms(u64::MAX);
        assert_eq!(far.as_epoch_ms_i64(), i64::MAX, "E2");
        assert_eq!(far.as_epoch_ms(), i64::MAX as u64, "E2");

        let nanos = 1_700_000_000_123_456_789_u128;
        let fine = UtcTime::from_unix_nanos(nanos);
        assert_eq!(fine.as_unix_nanos(), nanos, "E3");
        assert_eq!(fine.as_epoch_ms(), 1_700_000_000_123, "E3");

        let ms = 1_709_164_800_250_u64;
        let reading = UtcTime::from_epoch_ms(ms);
        assert_eq!(reading.as_epoch_ms(), ms, "E4");
        assert_eq!(reading.as_epoch_ms_i64(), ms as i64, "E4");
        assert_eq!(UtcTime::from_epoch_ms_i64(ms as i64), reading, "E4");
        assert_eq!(reading.as_unix_nanos(), u128::from(ms) * 1_000_000, "E4");
        assert_eq!(reading.to_rfc3339(), "2024-02-29T00:00:00.25Z", "E4");
        assert_eq!(
            UtcTime::parse_rfc3339(&reading.to_rfc3339()),
            Some(reading),
            "E4"
        );
    }

    #[test]
    fn rfc3339_spellings_come_from_the_one_codec() {
        // R1 the millisecond spelling equals the codec's; R2 the whole-second
        // spelling truncates (never rounds) and is fixed width; R3 the codec's
        // year-9999 ceiling bounds both; R4 parse rejects what the codec
        // rejects and accepts nanosecond precision.
        let reading = UtcTime::from_epoch_ms(1_800_000_000_999);
        assert_eq!(
            reading.to_rfc3339(),
            utc::epoch_millis_to_rfc3339(1_800_000_000_999),
            "R1"
        );
        assert_eq!(reading.to_rfc3339_secs(), "2027-01-15T08:00:00Z", "R2");
        assert_eq!(
            UtcTime::UNIX_EPOCH.to_rfc3339_secs(),
            "1970-01-01T00:00:00Z",
            "R2"
        );
        assert_eq!(reading.to_rfc3339_secs().len(), 20, "R2");
        let far = UtcTime::from_epoch_ms(u64::MAX);
        assert_eq!(far.to_rfc3339(), "9999-12-31T23:59:59.999Z", "R3");
        assert_eq!(far.to_rfc3339_secs(), "9999-12-31T23:59:59Z", "R3");
        assert_eq!(UtcTime::parse_rfc3339("2026-02-29T00:00:00Z"), None, "R4");
        assert_eq!(UtcTime::parse_rfc3339("1969-12-31T23:59:59Z"), None, "R4");
        assert_eq!(
            UtcTime::parse_rfc3339("1970-01-01T00:00:01.000000001Z"),
            Some(UtcTime::from_unix_nanos(1_000_000_001)),
            "R4"
        );
    }

    #[test]
    fn arithmetic_saturates_instead_of_wrapping() {
        // R1 adding past the Duration ceiling saturates; R2 a later minus an
        // earlier reading is exact; R3 an earlier minus a later is zero.
        let top = UtcTime::from_duration_since_epoch(Duration::MAX);
        assert_eq!(top.saturating_add(Duration::from_secs(1)), top, "R1");
        let a = UtcTime::from_epoch_ms(1_000);
        let b = UtcTime::from_epoch_ms(1_250);
        assert_eq!(
            b.saturating_duration_since(a),
            Duration::from_millis(250),
            "R2"
        );
        assert_eq!(a.saturating_duration_since(b), Duration::ZERO, "R3");
        assert_eq!(a.duration_since_epoch(), Duration::from_secs(1), "R2");
    }
}
