// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Both Session stores reuse this sole deletion-receipt codec under their root delete CAS.
// @formal reason: Adjacent codec tables reject corrupt identities, preserve legacy unknown versus
// @formal reason: known-empty evidence and retain only ordered opaque operation coordinates. This is
// @formal reason: product-requirement evidence, not a formal proof of SQL atomicity or tombstone
// @formal reason: retention.
use awaken_session_contract::RetainedEventAdmission;
use awaken_session_contract::{
    DeletedSessionEventAdmissions, PersistedSession, SessionRepositoryError,
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
                batch_id: Some(batch.batch_id.clone()),
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
                || receipt
                    .batch_id
                    .as_ref()
                    .is_some_and(|id| id.trim().is_empty())
                || (receipt.batch_id.is_none()
                    && (receipt.closed
                        || receipt
                            .operation_ids
                            .as_ref()
                            .is_none_or(|ids| !ids.is_empty())))
                || !keys.insert(&receipt.key)
            {
                return Err(super::corrupt(
                    "invalid retained Session event admission identity",
                ));
            }
            if let Some(ids) = &receipt.operation_ids
                && receipt.batch_id.is_some()
                && ((receipt.closed && !ids.is_empty())
                    || (!receipt.closed && ids.is_empty())
                    || ids.iter().enumerate().any(|(ordinal, id)| {
                        awaken_session_contract::decode_session_event_batch_operation(id)
                            .is_none_or(|coordinate| {
                                Some(coordinate.batch_id) != receipt.batch_id.as_deref()
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

/// Delete copies the canonical receipt-only facts under the same root lock.
/// No message, input payload or inferred empty legacy receipt is retained.
pub(super) fn merge_noops(
    encoded: &str,
    current_revision: SessionRevision,
    mut rows: Vec<(String, String, i64)>,
) -> Result<String, SessionRepositoryError> {
    let mut receipts: Vec<RetainedEventAdmission> =
        serde_json::from_str(encoded).map_err(super::corrupt)?;
    rows.sort_by(|left, right| (left.2, &left.0).cmp(&(right.2, &right.0)));
    for (key, payload_hash, revision) in rows {
        let record = awaken_session_contract::IdempotencyRecord { key, payload_hash };
        let receipt = super::decode_session_receipt(
            record.payload_hash.clone(),
            revision,
            super::receipt_kind_value(
                awaken_session_contract::SessionCommandReceiptKind::EventNoOp,
            ),
        )?;
        if !record.confirmed_by(&receipt, current_revision) {
            return Err(super::corrupt(
                "Event no-op receipt exceeds its root observation",
            ));
        }
        receipts.push(RetainedEventAdmission {
            key: record
                .event_admission_key()
                .ok_or_else(|| super::corrupt("Event no-op key has no Event namespace"))?
                .to_owned(),
            request_fingerprint: record.payload_hash,
            batch_id: None,
            closed: false,
            operation_ids: Some(Vec::new()),
        });
    }
    let encoded = serde_json::to_string(&receipts).map_err(super::corrupt)?;
    // The existing decoder owns uniqueness and complete/legacy shape checks.
    decode(super::db_revision(current_revision)?, Some(encoded.clone()))?;
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn noop_deletion_codec_requires_one_consistent_receipt_shape_and_visible_revision() {
        // C1 missing batch, open receipt and known-empty ids; C2 any mixed or
        // omitted field; C3 future/duplicate/wrong-namespace receipt. N1 C1 =>
        // retain exact no-op; N2 C2/C3 => Corrupt. No legacy absence becomes [].
        let valid = json!({"key":"k","request_fingerprint":"f","closed":false,"operation_ids":[]});
        assert!(
            decode(2, Some(json!([valid.clone()]).to_string()))
                .unwrap()
                .admissions
                .unwrap()[0]
                .batch_id
                .is_none()
        );
        for patch in [
            json!({"closed":true}),
            json!({"operation_ids":null}),
            json!({"operation_ids":["x"]}),
            json!({"batch_id":"fake-batch"}),
        ] {
            let mut row = valid.clone();
            for (key, value) in patch.as_object().unwrap() {
                row[key] = value.clone();
            }
            assert!(decode(2, Some(json!([row]).to_string())).is_err());
        }
        let key = awaken_session_contract::IdempotencyRecord::event_admission("k", "f").key;
        let encoded =
            merge_noops("[]", SessionRevision(2), vec![(key.clone(), "f".into(), 1)]).unwrap();
        assert_eq!(
            decode(3, Some(encoded)).unwrap().admissions.unwrap()[0].key,
            "k"
        );
        for rows in [
            vec![(key.clone(), "f".into(), 3)],
            vec![("root:key".into(), "f".into(), 1)],
            vec![(key.clone(), "f".into(), 1), (key.clone(), "f".into(), 1)],
        ] {
            assert!(merge_noops("[]", SessionRevision(2), rows).is_err());
        }
    }

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
            awaken_session_contract::session_event_batch_operation(batch, ordinal).unwrap()
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
