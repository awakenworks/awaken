// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Storage projection for the one Credential outbox: only acknowledged,
// @formal reason: exact current-format payloads enter the domain's scoped supersession
// @formal reason: decision. Pending, undecodable and future-shaped rows are retained.
// @formal reason: Adjacent decision-table tests and the shared repository conformance
// @formal reason: suite qualify these transitions; this is not a formal-proof claim.
//! Receipt pruning uses the Credential owner's one lineage/revision decision.
//! Undecodable or future-shaped records remain untouched diagnostic evidence.

use awaken_credential_contract::{
    ManagedCredentialRollout, managed_rollout_acknowledgement_superseded_by,
};

pub(crate) fn obsolete_acknowledgements(rows: Vec<(String, String)>) -> Vec<String> {
    let events = rows
        .into_iter()
        .filter_map(|(id, data)| {
            let value: serde_json::Value = serde_json::from_str(&data).ok()?;
            let event: ManagedCredentialRollout = serde_json::from_value(value.clone()).ok()?;
            // A future extension must not disappear through a permissive Serde
            // projection. Only an exact current-format round trip permits pruning.
            (id == event.id && serde_json::to_value(&event).ok().as_ref() == Some(&value))
                .then_some(event)
        })
        .collect::<Vec<_>>();
    events
        .iter()
        .filter(|older| {
            events
                .iter()
                .any(|newer| managed_rollout_acknowledgement_superseded_by(older, newer))
        })
        .map(|event| event.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_contract::{CredentialSourceId, ManagedCredentialOperation};

    fn event(id: &str, version: u64) -> ManagedCredentialRollout {
        ManagedCredentialRollout {
            id: id.into(),
            workspace_id: "w".into(),
            vault_id: "v".into(),
            credential_id: "c".into(),
            source_id: CredentialSourceId("s".into()),
            source_version: version,
            credential_revision: version,
            operation: ManagedCredentialOperation::Update,
        }
    }

    #[test]
    fn receipt_pruning_preserves_future_malformed_mismatched_and_foreign_records() {
        // C1 older acknowledged current-format row; C2 newer same lineage;
        // C3 malformed/future JSON, row/event id mismatch, or foreign scope.
        // E1 only C1+C2 can be pruned; E2 every C3 row is retained AND cannot
        // serve as a newer witness that deletes a valid current receipt.
        let old = event("old", 2);
        let new = event("new", 3);
        let mut future = serde_json::to_value(event("future", 99)).unwrap();
        future["future_contract"] = serde_json::json!({"version":2});
        let mut foreign = event("foreign", 99);
        foreign.workspace_id = "other".into();
        let row = |x: &ManagedCredentialRollout| (x.id.clone(), serde_json::to_string(x).unwrap());
        let rows = vec![
            row(&old),
            row(&new),
            ("malformed".into(), "{".into()),
            ("future".into(), future.to_string()),
            (
                "alias".into(),
                serde_json::to_string(&event("other-id", 100)).unwrap(),
            ),
            row(&foreign),
        ];
        assert_eq!(obsolete_acknowledgements(rows), vec!["old".to_owned()]);
    }
}
