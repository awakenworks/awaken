//! Ephemeral Kubernetes Create broker payloads; object bytes may hold Secrets.
//! They are never durable ticket, debug or error values.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sCreateRequest {
    pub path: String,
    pub object: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sCreateResponse {
    pub status: u16,
    pub object: Value,
}
