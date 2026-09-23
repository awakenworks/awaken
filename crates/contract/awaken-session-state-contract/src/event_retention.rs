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
