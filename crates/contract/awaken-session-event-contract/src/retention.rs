//! Minimal Event admission receipts retained after Session deletion.

use awaken_session_types::SessionRevision;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedEventAdmission {
    pub key: String,
    pub request_fingerprint: String,
    pub batch_id: String,
    pub closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_ids: Option<Vec<String>>,
}

/// Durable deletion evidence. `None` is a legacy tombstone with unknown
/// retention; `Some(empty)` proves no keyed batch was accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedSessionEventAdmissions {
    pub deleted_revision: SessionRevision,
    pub admissions: Option<Vec<RetainedEventAdmission>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_receipt_keeps_legacy_bytes_and_unknown_admission_distinct_from_empty() {
        // W1 legacy receipts omit operation ids; W2 known empty ids retain [];
        // W3 an unknown field is rejected; W4 unknown retention never becomes
        // proof that no batch was accepted after moving the vocabulary owner.
        let legacy = r#"{"key":"key","request_fingerprint":"fp","batch_id":"batch","closed":true}"#;
        let receipt: RetainedEventAdmission = serde_json::from_str(legacy).unwrap();
        assert_eq!(receipt.operation_ids, None);
        assert_eq!(serde_json::to_string(&receipt).unwrap(), legacy);
        let mut known = receipt.clone();
        known.operation_ids = Some(Vec::new());
        assert_eq!(
            serde_json::to_value(&known).unwrap()["operation_ids"],
            serde_json::json!([])
        );
        let mut unknown_field = serde_json::to_value(&receipt).unwrap();
        unknown_field["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<RetainedEventAdmission>(unknown_field).is_err());
        assert_ne!(
            DeletedSessionEventAdmissions {
                deleted_revision: SessionRevision(7),
                admissions: None
            },
            DeletedSessionEventAdmissions {
                deleted_revision: SessionRevision(7),
                admissions: Some(Vec::new())
            },
        );
    }
}
