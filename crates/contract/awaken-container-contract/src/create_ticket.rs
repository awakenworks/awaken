// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Pure Create-ledger ticket wire values. The guarded-claim handle persists one
// @formal reason: ticket and the Kubernetes backend's ledger orders their transitions; both consume
// @formal reason: this sole definition. No runtime or state service is introduced.
//! The Kubernetes Create-ledger ticket: one admitted Create request and its
//! definitive result.
//!
//! The Container handle codec, Kubernetes ledger and independent Broker consume
//! these same contract values. Backend transitions and physical authority remain
//! in the existing Kubernetes adapter. A ticket is not a cleanup receipt.
use serde::{Deserialize, Serialize};

use crate::RuntimeError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum TicketResult {
    Created {
        uid: String,
    },
    RejectedNoEffect {
        code: u16,
    },
    /// A provider-owned inert object occupies the exact Create name.
    /// The independent close reader must reverify its UID and protection
    /// before this result can satisfy any Session terminal obligation.
    Isolated {
        uid: String,
    },
}

impl TicketResult {
    /// Validate the existing definitive-result shape. This does not establish
    /// original request identity, isolation purpose or physical closure; those
    /// remain with the caller and the ledger's controlled transitions.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        match self {
            Self::Created { uid } | Self::Isolated { uid }
                if !uid.trim().is_empty() && uid.trim() == uid =>
            {
                Ok(())
            }
            Self::RejectedNoEffect { code } if matches!(*code, 400 | 401 | 403 | 409 | 422) => {
                Ok(())
            }
            _ => Err(RuntimeError::Backend(
                "Kubernetes Create result is not a definitive physical observation".into(),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum TicketStatus {
    InFlight,
    Finished(TicketResult),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTicketRequest {
    pub group: String,
    pub resource: String,
    pub namespace: String,
    pub name: String,
    pub digest: String,
}

impl CreateTicketRequest {
    pub fn new(
        group: impl Into<String>,
        resource: impl Into<String>,
        namespace: impl Into<String>,
        name: impl Into<String>,
        digest: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let request = Self {
            group: group.into(),
            resource: resource.into(),
            namespace: namespace.into(),
            name: name.into(),
            digest: digest.into(),
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        let fields = [
            (self.group.as_str(), 256),
            (self.resource.as_str(), 64),
            (self.namespace.as_str(), 253),
            (self.name.as_str(), 253),
        ];
        if fields
            .into_iter()
            .any(|(value, max)| value.is_empty() || value.len() > max || value.trim() != value)
            || !supported_resource(&self.resource)
            || self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(RuntimeError::Backend(
                "Kubernetes Create ticket has incomplete or unbounded identity".into(),
            ));
        }
        Ok(())
    }

    pub fn key(&self) -> String {
        let encoded =
            serde_json::to_vec(&(&self.group, &self.resource, &self.namespace, &self.name))
                .expect("validated Create ticket identity is serializable");
        blake3::hash(&encoded).to_hex().to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTicket {
    pub request: CreateTicketRequest,
    pub status: TicketStatus,
    /// Worker requests and terminal name reservations share this one ledger.
    /// Historical tickets default to the original Worker request origin.
    #[serde(default)]
    pub origin: TicketOrigin,
    /// Minted by the provider's first CAS admission, never by the Worker.
    /// Historical tickets without it remain Unknown after process loss.
    #[serde(default)]
    pub isolation_id: String,
    /// CASed before a provider-owned reservation POST. Even if the original
    /// Create records a definitive result, join waits for this POST to settle.
    #[serde(default)]
    pub isolation_pending: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketOrigin {
    #[default]
    Worker,
    TerminalGuard,
}

fn supported_resource(value: &str) -> bool {
    matches!(
        value,
        "pods" | "persistentvolumeclaims" | "configmaps" | "secrets" | "jobs" | "daemonsets"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TicketAdmission {
    Forward,
    InFlight,
    Finished(TicketResult),
    Sealed,
}

/// Complete means only that no earlier admitted Create request can still
/// materialize. Participant absence/disposal remains a separate provider fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum RequestJoin {
    Unknown,
    Complete(Vec<CreateTicket>),
}

/// Registry-frozen witness plus the exact retained Worker Pod identity. The
/// provider verifies both before every ticket CAS; a stable ConfigMap name is
/// never accepted in place of the original UID.
#[derive(Clone, Copy)]
pub struct K8sLedgerBinding<'a> {
    pub worker_namespace: &'a str,
    pub worker_name: &'a str,
    pub worker_uid: &'a str,
    pub ledger_name: &'a str,
    pub ledger_uid: &'a str,
    pub configuration_fingerprint: &'a str,
}
