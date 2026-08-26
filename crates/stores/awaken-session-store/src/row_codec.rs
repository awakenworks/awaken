//! Canonical aggregate decoding shared by SQLite and PostgreSQL.
//!
//! The aggregate is the sole durable Session model. Indexed SQL columns are
//! projections for lookup and constraints; they are never an alternate source
//! from which a partially specified aggregate can be reconstructed.

use awaken_session_contract::{PersistedSession, SessionRevision};

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
    let mut aggregate: PersistedSession = serde_json::from_str(&row.aggregate_json)?;
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
}
