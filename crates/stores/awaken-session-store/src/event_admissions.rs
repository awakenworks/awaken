use awaken_session_contract::{
    DeletedSessionEventAdmissions, PersistedSession, RetainedEventAdmission,
    SessionRepositoryError, SessionRevision,
};

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
}
