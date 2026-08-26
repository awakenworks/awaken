//! Canonical aggregate decoding shared by SQLite and PostgreSQL.
//!
//! The aggregate is the sole durable Session model. Indexed SQL columns are
//! projections for lookup and constraints; they are never an alternate source
//! from which a partially specified aggregate can be reconstructed.

use awaken_session_contract::{PersistedSession, SessionRevision};

const CURRENT_AGGREGATE_FORMAT: &str = "awaken.session.v1";

#[derive(serde::Serialize)]
#[serde(deny_unknown_fields)]
struct AggregateEnvelope<'a> {
    format: &'static str,
    aggregate: &'a PersistedSession,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedAggregateEnvelope {
    format: String,
    aggregate: PersistedSession,
}

pub(super) fn encode(session: &PersistedSession) -> Result<String, serde_json::Error> {
    serde_json::to_string(&AggregateEnvelope {
        format: CURRENT_AGGREGATE_FORMAT,
        aggregate: session,
    })
}

fn invalid(message: impl Into<String>) -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message.into(),
    ))
}

fn decode_aggregate(data: &str) -> Result<PersistedSession, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(data)?;
    if value.get("format").is_some() || value.get("aggregate").is_some() {
        let envelope: OwnedAggregateEnvelope = serde_json::from_value(value)?;
        if envelope.format != CURRENT_AGGREGATE_FORMAT {
            return Err(invalid(format!(
                "unsupported managed Session aggregate format `{}`",
                envelope.format
            )));
        }
        return Ok(envelope.aggregate);
    }

    // Pre-envelope aggregates from before the root event-batch convergence did
    // not contain either field. They had no root event-batch provenance to
    // preserve, so the exact migration is two empty collections. Both fields
    // were introduced atomically; a row missing only one is corruption rather
    // than a recognized historical format and remains fail-closed.
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("managed Session aggregate must be an object"))?;
    match (
        object.contains_key("event_batches"),
        object.contains_key("active_activity_epochs"),
    ) {
        (false, false) => {
            object.insert("event_batches".into(), serde_json::json!([]));
            object.insert("active_activity_epochs".into(), serde_json::json!([]));
        }
        (true, true) => {}
        _ => {
            return Err(invalid(
                "managed Session aggregate has a partial event-batch schema",
            ));
        }
    }
    serde_json::from_value(value)
}

pub(super) struct EncodedSessionRow {
    pub aggregate_json: String,
    pub revision: i64,
}

pub(super) fn decode(row: EncodedSessionRow) -> Result<PersistedSession, serde_json::Error> {
    let revision = SessionRevision(u64::try_from(row.revision).map_err(|_| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed Session revision is negative",
        ))
    })?);
    let mut aggregate = decode_aggregate(&row.aggregate_json)?;
    if aggregate.revision != revision {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed Session aggregate revision does not match its index",
        )));
    }
    aggregate.revision = revision;
    Ok(aggregate)
}

#[cfg(test)]
mod tests {
    use awaken_session_contract::ManagedSessionRepository as _;
    use rusqlite::params;

    use crate::{SqliteManagedSessionRepository, tests::create_fixture, tests::sample};

    #[tokio::test]
    async fn canonical_aggregate_and_index_revision_are_cross_validated() {
        // Boundary partition: C1 canonical aggregate + matching index -> accept;
        // C2 aggregate/index revision drift -> reject. NOT NULL makes a missing
        // aggregate structurally unrepresentable in the current baseline.
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        create_fixture(&repo, "default", sample("strict-row"), Vec::new()).await;
        assert!(repo.get("strict-row").await.is_ok(), "C1");

        create_fixture(&repo, "default", sample("drifted-row"), Vec::new()).await;
        repo.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE managed_session SET revision = revision + 1 WHERE session_id = ?1",
                params!["drifted-row"],
            )
            .unwrap();
        assert!(repo.get("drifted-row").await.is_err(), "C2");
    }

    #[test]
    fn aggregate_format_decision_table_migrates_only_the_known_legacy_shape() {
        // | Rule | envelope | event_batches | active epochs | Effect |
        // | F1   | v1       | present       | present       | accept |
        // | F2   | absent   | absent        | absent        | migrate empty |
        // | F3   | absent   | present       | absent        | reject |
        // | F4   | unknown  | present       | present       | reject |
        let session = sample("format-matrix");
        let canonical = super::encode(&session).expect("encode v1 envelope");
        assert_eq!(
            super::decode_aggregate(&canonical).expect("F1"),
            session,
            "F1"
        );

        let mut legacy = serde_json::to_value(&session).expect("legacy aggregate JSON");
        legacy.as_object_mut().unwrap().remove("event_batches");
        legacy
            .as_object_mut()
            .unwrap()
            .remove("active_activity_epochs");
        assert_eq!(
            super::decode_aggregate(&legacy.to_string()).expect("F2"),
            session,
            "F2"
        );

        legacy["event_batches"] = serde_json::json!([]);
        assert!(super::decode_aggregate(&legacy.to_string()).is_err(), "F3");

        let mut unknown: serde_json::Value =
            serde_json::from_str(&canonical).expect("canonical envelope JSON");
        unknown["format"] = serde_json::json!("awaken.session.v999");
        assert!(super::decode_aggregate(&unknown.to_string()).is_err(), "F4");
    }
}
