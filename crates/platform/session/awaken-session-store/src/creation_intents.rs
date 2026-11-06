// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: One private row codec for both creation-intent adapters; the
// @formal reason: domain owns the phases, while each adapter retains its SQL locks.
//! Shared storage encoding for the existing pre-root creation intent.

use awaken_session_contract::{PreparedParticipantIntent, PreparedParticipantIntentPhase};

use crate::{SessionRepositoryError, corrupt};

pub(crate) const PREPARING: &str = "preparing";
pub(crate) const ABORTING: &str = "aborting";
pub(crate) const ABORTED: &str = "aborted";
pub(crate) type IntentRow = (String, String, String, String, i64);

pub(crate) fn decode_phase(
    phase: &str,
) -> Result<PreparedParticipantIntentPhase, SessionRepositoryError> {
    match phase {
        PREPARING => Ok(PreparedParticipantIntentPhase::Preparing),
        ABORTING => Ok(PreparedParticipantIntentPhase::Aborting),
        ABORTED => Ok(PreparedParticipantIntentPhase::Aborted),
        other => Err(corrupt(format!("unknown creation intent phase `{other}`"))),
    }
}

pub(crate) fn decode_intent(
    row: IntentRow,
) -> Result<PreparedParticipantIntent, SessionRepositoryError> {
    let (session_id, owner_scope, phase, repository_ids, lease) = row;
    let phase = decode_phase(&phase)?;
    Ok(PreparedParticipantIntent::new(
        session_id.into(),
        owner_scope,
        serde_json::from_str(&repository_ids).map_err(corrupt)?,
        phase,
        u64::try_from(lease).map_err(corrupt)?,
    ))
}

pub(crate) fn db_ms(value: u64) -> Result<i64, SessionRepositoryError> {
    i64::try_from(value).map_err(|_| {
        SessionRepositoryError::InvalidMutation("creation intent lease is out of range".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_adapters_decode_only_known_phases_valid_ids_and_nonnegative_leases() {
        // DT-C: C1 known phase; C2 JSON string-vector; C3 nonnegative lease.
        // C1 AND C2 AND C3 => exact intent coordinates; every other row =>
        // Corrupt. Exhaust all 4 phase x 2 JSON x 2 lease rows through the
        // shared storage boundary; no alternate test decoder or state owner.
        for phase in [PREPARING, ABORTING, ABORTED, "unknown"] {
            for ids in [r#"["repository"]"#, "{}"] {
                for lease in [0, -1] {
                    let decoded = decode_intent((
                        "session".into(),
                        "workspace".into(),
                        phase.into(),
                        ids.into(),
                        lease,
                    ));
                    if phase != "unknown" && ids != "{}" && lease >= 0 {
                        let intent = decoded.unwrap();
                        assert_eq!(intent.session_id().as_str(), "session");
                        assert_eq!(intent.owner_scope(), "workspace");
                        assert_eq!(intent.repository_ids(), &["repository"]);
                        assert_eq!(intent.lease_expires_at_unix_ms(), 0);
                        assert_eq!(
                            intent.phase(),
                            if phase == PREPARING {
                                PreparedParticipantIntentPhase::Preparing
                            } else if phase == ABORTING {
                                PreparedParticipantIntentPhase::Aborting
                            } else {
                                PreparedParticipantIntentPhase::Aborted
                            }
                        );
                    } else {
                        assert!(matches!(decoded, Err(SessionRepositoryError::Corrupt(_))));
                    }
                }
            }
        }
    }

    #[test]
    fn lease_encoding_preserves_both_endpoints_and_refuses_overflow() {
        // DT-L: signed storage range includes 0 and i64::MAX; the first
        // unsigned value outside it must be refused before either SQL write.
        for value in [0, i64::MAX as u64] {
            assert_eq!(db_ms(value).unwrap() as u64, value);
        }
        for value in [i64::MAX as u64 + 1, u64::MAX] {
            assert!(matches!(
                db_ms(value),
                Err(SessionRepositoryError::InvalidMutation(_))
            ));
        }
    }
}
