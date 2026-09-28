// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Both Session stores reuse this sole deletion-receipt codec under their root delete CAS.
// @formal reason: Adjacent codec tables reject corrupt identities, preserve legacy unknown versus
// @formal reason: known-empty evidence and retain only ordered opaque operation coordinates. This is
// @formal reason: product-requirement evidence, not a formal proof of SQL atomicity or tombstone
// @formal reason: retention.
use awaken_session_state_contract::{
    DeletedSessionEventAdmissions, PersistedSession, RetainedEventAdmission, SessionRepositoryError,
};
use awaken_session_types::SessionRevision;

/// Both stores call this on the root they locked for the successful delete CAS,
/// never on a caller-provided inventory. Drop all original message/tool material.
pub(super) fn encode(session: &PersistedSession) -> Result<String, SessionRepositoryError> {
    let mut receipts = Vec::new();
    for batch in &session.event_batches {
        match (&batch.idempotency_key, &batch.request_fingerprint) {
            (Some(key), Some(fingerprint)) => receipts.push(RetainedEventAdmission {
                key: key.clone(),
                request_fingerprint: fingerprint.clone(),
                batch_id: batch.batch_id.clone(),
                closed: batch.admission_closed,
                operation_ids: Some(batch.operation_ids()),
            }),
            (None, None) => {}
            _ => {
                return Err(super::corrupt(
                    "incomplete Session event admission identity",
                ));
            }
        }
    }
    serde_json::to_string(&receipts).map_err(super::corrupt)
}

pub(super) fn decode(
    revision: i64,
    encoded: Option<String>,
) -> Result<DeletedSessionEventAdmissions, SessionRepositoryError> {
    let deleted_revision = SessionRevision(u64::try_from(revision).map_err(super::corrupt)?);
    if deleted_revision.0 == 0 {
        return Err(super::corrupt("invalid deleted Session revision"));
    }
    let admissions: Option<Vec<RetainedEventAdmission>> = encoded
        .map(|data| serde_json::from_str(&data).map_err(super::corrupt))
        .transpose()?;
    let mut keys = std::collections::BTreeSet::new();
    if let Some(receipts) = &admissions {
        for receipt in receipts {
            if receipt.key.trim().is_empty()
                || receipt.request_fingerprint.trim().is_empty()
                || receipt.batch_id.trim().is_empty()
                || !keys.insert(&receipt.key)
            {
                return Err(super::corrupt(
                    "invalid retained Session event admission identity",
                ));
            }
            if let Some(ids) = &receipt.operation_ids
                && ((receipt.closed && !ids.is_empty())
                    || (!receipt.closed && ids.is_empty())
                    || ids.iter().enumerate().any(|(ordinal, id)| {
                        awaken_session_event_contract::decode_session_event_batch_operation(id)
                            .is_none_or(|coordinate| {
                                coordinate.batch_id != receipt.batch_id
                                    || coordinate.ordinal != ordinal
                            })
                    }))
            {
                return Err(super::corrupt(
                    "invalid retained Session event observation identity",
                ));
            }
        }
    }
    Ok(DeletedSessionEventAdmissions {
        deleted_revision,
        admissions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retained_proof_codec_rejects_ambiguous_or_corrupt_evidence() {
        // C1 positive revision, C2 nullable legacy or complete JSON array,
        // C3 unique nonempty identity, C4 no payload fields. R1 all=>decode;
        // R2 NULL=>unknown, not []; R3 any !C1/!C2/!C3/!C4=>Corrupt.
        assert!(decode(1, None).unwrap().admissions.is_none());
        assert_eq!(
            decode(1, Some("[]".into())).unwrap().admissions,
            Some(vec![])
        );
        for revision in [-1, 0] {
            assert!(decode(revision, Some("[]".into())).is_err());
        }
        let valid = json!({"key":"k","request_fingerprint":"f","batch_id":"b","closed":false});
        assert!(decode(1, Some(json!([valid.clone()]).to_string())).is_ok());
        let mut invalid = vec![
            "null".into(),
            "{}".into(),
            "broken".into(),
            json!([valid.clone(), valid.clone()]).to_string(),
        ];
        for field in ["key", "request_fingerprint", "batch_id"] {
            let mut entry = valid.clone();
            entry[field] = json!(" ");
            invalid.push(json!([entry]).to_string());
        }
        let mut extra = valid;
        extra["content"] = json!("must not survive deletion");
        invalid.push(json!([extra]).to_string());
        for encoded in invalid {
            assert!(matches!(
                decode(1, Some(encoded)),
                Err(SessionRepositoryError::Corrupt(_))
            ));
        }
    }

    #[test]
    fn retained_observation_coordinates_are_complete_ordered_and_content_free() {
        // I1 omitted identities on an old receipt => preserve unknown, not [];
        // I2 closed => known empty; I3 accepted => ordered canonical batch
        // coordinates; I4 empty/foreign/gapped/repeated/noncanonical => corrupt.
        // Only opaque ids extend the existing tombstone JSON, never input data.
        let op = |batch, ordinal| {
            awaken_session_event_contract::session_event_batch_operation(batch, ordinal).unwrap()
        };
        let entry = |closed, ids: serde_json::Value| {
            json!([{
            "key":"k", "request_fingerprint":"f", "batch_id":"b", "closed":closed, "operation_ids":ids,
        }]).to_string()
        };
        assert_eq!(
            decode(1, Some(entry(true, json!([]))))
                .unwrap()
                .admissions
                .unwrap()[0]
                .operation_ids,
            Some(vec![])
        );
        let ids = vec![op("b", 0), op("b", 1)];
        assert_eq!(
            decode(1, Some(entry(false, json!(ids))))
                .unwrap()
                .admissions
                .unwrap()[0]
                .operation_ids,
            Some(ids)
        );
        for (closed, ids) in [
            (false, json!([])),
            (true, json!([op("b", 0)])),
            (false, json!([op("foreign", 0)])),
            (false, json!([op("b", 1)])),
            (false, json!([op("b", 0), op("b", 0)])),
            (false, json!([" "])),
            (false, json!(["operation:private message"])),
        ] {
            assert!(
                matches!(
                    decode(1, Some(entry(closed, ids))),
                    Err(SessionRepositoryError::Corrupt(_))
                ),
                "I4"
            );
        }
    }
}
