//! The original Envelope and Custody fixtures used by the Vault wire tests.

use std::sync::Mutex;

#[derive(Default)]
pub(super) struct RecordingEnvelopeIssuer {
    pub(super) issued: Mutex<Vec<(String, u64, String)>>,
}

#[derive(Default)]
pub(super) struct RecordingCustodian {
    pub(super) published: Mutex<Vec<(String, u64, String)>>,
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialMaterialCustodian for RecordingCustodian {
    fn handles(
        &self,
        _selected_holder: &awaken_credential_contract::PlaintextHolder,
        _usage: &awaken_credential_contract::CredentialUsage,
    ) -> bool {
        true
    }

    async fn publish(
        &self,
        publication: awaken_credential_contract::CredentialCustodyPublication,
    ) -> Result<(), String> {
        self.published.lock().unwrap().push((
            publication.access.credential.id,
            publication.access.credential.revision,
            publication.material.expose_secret().to_owned(),
        ));
        Ok(())
    }
}

pub(super) struct SelectiveCustodian {
    pub(super) holder: awaken_credential_contract::PlaintextHolder,
    pub(super) usage: awaken_credential_contract::CredentialUsage,
    pub(super) published: Mutex<usize>,
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialMaterialCustodian for SelectiveCustodian {
    fn handles(
        &self,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        usage: &awaken_credential_contract::CredentialUsage,
    ) -> bool {
        selected_holder == &self.holder && usage == &self.usage
    }

    async fn publish(
        &self,
        _publication: awaken_credential_contract::CredentialCustodyPublication,
    ) -> Result<(), String> {
        *self.published.lock().unwrap() += 1;
        Ok(())
    }
}

pub(super) enum AdversarialEnvelopeResponse {
    SubstitutedPayload,
    Failure,
}

pub(super) struct AdversarialEnvelopeIssuer(pub(super) AdversarialEnvelopeResponse);

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialEnvelopeIssuer for RecordingEnvelopeIssuer {
    async fn issue(
        &self,
        request: awaken_credential_contract::CredentialEnvelopeIssuance,
    ) -> Result<awaken_credential_contract::CredentialEnvelope, String> {
        self.issued.lock().unwrap().push((
            request.access.credential.id.clone(),
            request.access.credential.revision,
            request.material.expose_secret().to_string(),
        ));
        Ok(
            awaken_credential_contract::CredentialEnvelope::SealedForWorker {
                envelope_ref: awaken_credential_contract::SealedCredentialEnvelopeRef {
                    id: "test-envelope".into(),
                    payload_fingerprint:
                        awaken_credential_contract::credential_envelope_payload_fingerprint(
                            &request.access,
                            &request.selected_holder,
                            &request.binding,
                        ),
                },
                recipient: request.selected_holder.trust_domain,
                expires_at_unix_ms: u64::MAX,
            },
        )
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialEnvelopeIssuer for AdversarialEnvelopeIssuer {
    async fn issue(
        &self,
        request: awaken_credential_contract::CredentialEnvelopeIssuance,
    ) -> Result<awaken_credential_contract::CredentialEnvelope, String> {
        match self.0 {
            AdversarialEnvelopeResponse::Failure => Err("issuer unavailable".into()),
            AdversarialEnvelopeResponse::SubstitutedPayload => Ok(
                awaken_credential_contract::CredentialEnvelope::SealedForWorker {
                    envelope_ref: awaken_credential_contract::SealedCredentialEnvelopeRef {
                        id: "substituted-envelope".into(),
                        payload_fingerprint: "substituted-payload".into(),
                    },
                    recipient: request.selected_holder.trust_domain,
                    expires_at_unix_ms: u64::MAX,
                },
            ),
        }
    }
}
