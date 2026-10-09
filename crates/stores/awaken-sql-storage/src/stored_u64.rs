// @formal evidence config.generation_is_monotonic
// @formal evidence config.stale_writer_never_applies
// @formal entrypoint work.persisted_epoch_is_exact_nonnegative_and_nonwrapping
//! The one driver-independent `u64` <-> SQL `BIGINT` codec.
//! Config, File, Memory, Work and commit stores all use this exact boundary;
//! adapters own their contextual errors, never another conversion law.

/// The only representation permitted at a `u64`/SQL `BIGINT` authority
/// boundary. SQL backends are signed; silently casting either direction can
/// turn overflow into a negative fence or corruption into a huge valid cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoredU64(i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoredU64Error {
    #[error("durable authority value {0} is negative")]
    Negative(i64),
    #[error("durable authority value {0} exceeds signed database range")]
    OutOfRange(u64),
    #[error("durable authority arithmetic overflow")]
    ArithmeticOverflow,
}

impl StoredU64 {
    #[must_use]
    pub const fn database_value(self) -> i64 {
        self.0
    }

    #[must_use]
    pub const fn domain_value(self) -> u64 {
        self.0 as u64
    }

    pub fn checked_add(self, increment: u64) -> Result<Self, StoredU64Error> {
        let value = self
            .domain_value()
            .checked_add(increment)
            .ok_or(StoredU64Error::ArithmeticOverflow)?;
        Self::try_from(value)
    }

    pub fn checked_scale_and_offset(
        self,
        scale: u64,
        offset: usize,
    ) -> Result<Self, StoredU64Error> {
        let offset = u64::try_from(offset).map_err(|_| StoredU64Error::ArithmeticOverflow)?;
        let value = self
            .domain_value()
            .checked_mul(scale)
            .and_then(|value| value.checked_add(offset))
            .ok_or(StoredU64Error::ArithmeticOverflow)?;
        Self::try_from(value)
    }
}

impl TryFrom<u64> for StoredU64 {
    type Error = StoredU64Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        i64::try_from(value)
            .map(Self)
            .map_err(|_| StoredU64Error::OutOfRange(value))
    }
}

impl TryFrom<i64> for StoredU64 {
    type Error = StoredU64Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < 0 {
            Err(StoredU64Error::Negative(value))
        } else {
            Ok(Self(value))
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{StoredU64, StoredU64Error};

    // Consolidates the five identical context-local suites without changing
    // their input partitions. Causes: C1 SQL sign; C2 unsigned range. Effects:
    // B1 negative => Negative; B2 zero/MAX => exact round-trip; B3 MAX+1 =>
    // OutOfRange. The property below additionally preserves arithmetic P4/P5.
    #[test]
    fn sql_bigint_codec_boundaries_are_exact() {
        assert_eq!(
            StoredU64::try_from(-1_i64),
            Err(StoredU64Error::Negative(-1))
        );
        assert_eq!(StoredU64::try_from(0_i64).unwrap().domain_value(), 0);
        assert_eq!(
            StoredU64::try_from(i64::MAX).unwrap().domain_value(),
            i64::MAX as u64
        );
        assert!(matches!(
            StoredU64::try_from(i64::MAX as u64 + 1),
            Err(StoredU64Error::OutOfRange(_))
        ));
    }

    proptest! {
        /// Cause/effect graph: C1 a SQL value is negative or non-negative; C2 a
        /// domain value fits or exceeds `i64::MAX`; C3 arithmetic stays in range
        /// or crosses it. Effects are E1 exact round-trip or E2 a typed error with
        /// no clamping, wrapping, or default substitution.
        ///
        /// | Rule | SQL sign | domain range | arithmetic | Effect |
        /// | P1 | non-negative | fits | n/a | E1 exact round-trip |
        /// | P2 | negative | n/a | n/a | E2 Negative |
        /// | P3 | n/a | exceeds | n/a | E2 OutOfRange |
        /// | P4 | n/a | fits | result fits | E1 exact result |
        /// | P5 | n/a | fits | result exceeds/overflows | E2 typed error |
        #[test]
        fn sql_bigint_codec_matches_checked_integer_semantics(
            signed in any::<i64>(),
            domain in any::<u64>(),
            increment in any::<u64>(),
            scale in any::<u64>(),
            offset in any::<usize>(),
        ) {
            match StoredU64::try_from(signed) {
                Ok(stored) => {
                    prop_assert!(signed >= 0, "P1");
                    prop_assert_eq!(stored.database_value(), signed, "P1");
                    prop_assert_eq!(stored.domain_value(), signed as u64, "P1");
                }
                Err(error) => {
                    prop_assert!(signed < 0, "P2");
                    prop_assert_eq!(error, StoredU64Error::Negative(signed), "P2");
                }
            }

            match StoredU64::try_from(domain) {
                Ok(stored) => {
                    prop_assert!(domain <= i64::MAX as u64, "P1/P4/P5");
                    prop_assert_eq!(stored.domain_value(), domain, "P1");
                    let expected_add = domain
                        .checked_add(increment)
                        .filter(|value| *value <= i64::MAX as u64);
                    prop_assert_eq!(
                        stored.checked_add(increment).ok().map(StoredU64::domain_value),
                        expected_add,
                        "P4/P5"
                    );
                    let expected_scaled = u64::try_from(offset)
                        .ok()
                        .and_then(|offset| domain.checked_mul(scale)?.checked_add(offset))
                        .filter(|value| *value <= i64::MAX as u64);
                    prop_assert_eq!(
                        stored.checked_scale_and_offset(scale, offset).ok().map(StoredU64::domain_value),
                        expected_scaled,
                        "P4/P5"
                    );
                }
                Err(error) => {
                    prop_assert!(domain > i64::MAX as u64, "P3");
                    prop_assert_eq!(error, StoredU64Error::OutOfRange(domain), "P3");
                }
            }
        }
    }
}
