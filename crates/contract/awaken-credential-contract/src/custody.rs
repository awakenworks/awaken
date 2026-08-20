//! Exact publication into an external credential custody boundary.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;

use crate::{
    CredentialAccess, CredentialEnvelopeIssuer, CredentialMaterialBinding, PlaintextHolder,
};

/// The one plaintext delivery mechanism selected by a product composition.
///
/// Envelope issuance and external custody are alternatives, not independent
/// observers. Encoding that choice as an enum prevents one resolved secret
/// from being released across two infrastructure boundaries.
#[derive(Clone)]
pub enum CredentialMaterialDelivery {
    RecipientEnvelope(Arc<dyn CredentialEnvelopeIssuer>),
    ExternalCustody(Arc<dyn CredentialMaterialCustodian>),
}

/// One exact, already-authorized publication into an external plaintext
/// custody boundary. The caller remains the credential authority and supplies
/// the immutable revision and binding; the implementation must be idempotent
/// for that coordinate and must not retain the plaintext in process memory.
pub struct CredentialCustodyPublication {
    pub access: CredentialAccess,
    pub selected_holder: PlaintextHolder,
    pub binding: CredentialMaterialBinding,
    pub material: RedactedString,
}

#[async_trait]
pub trait CredentialMaterialCustodian: Send + Sync {
    /// Returns whether this custody boundary owns the selected plaintext path.
    /// A custodian must not observe material for paths it does not own.
    fn handles(&self, selected_holder: &PlaintextHolder, usage: &crate::CredentialUsage) -> bool;

    async fn publish(&self, publication: CredentialCustodyPublication) -> Result<(), String>;
}
