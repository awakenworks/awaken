//! Self-hosted recipient-key transport for the existing Credential envelope.
//!
//! This adapter owns no Vault, credential selection, queue, or material store.
//! Operators project separate recipient keys into Control and the authorized
//! execution boundary. Ciphertext travels in the existing opaque envelope ref.
//! The key document is reread at issuance/use; removed keys never fall back to
//! another key or a local Vault. Live credential revocation remains the existing
//! Credential rollout/attempt fence, independently of transport-key revocation.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialAccess, CredentialEnvelope, CredentialEnvelopeIssuance, CredentialEnvelopeIssuer,
    CredentialMaterial, CredentialMaterialBinding, CredentialMaterialCapability,
    CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
    CredentialMaterialSource, PlaintextBoundary, PlaintextHolder, ResolvedCredentialMaterial,
    SealedCredentialEnvelopeRef, credential_envelope_payload_fingerprint,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde::{Deserialize, Serialize};

const PREFIX: &str = "awaken-recipient:v1:";
const MAX_BYTES: usize = 64 * 1024;
const MAX_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Deployment data, never a rendered configuration report or Debug value.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyDocument {
    ttl_seconds: u64,
    keys: Vec<RecipientKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipientKey {
    id: String,
    holder: PlaintextHolder,
    key_base64: String,
}

impl RecipientKey {
    fn cipher(&self) -> Result<ChaCha20Poly1305, CredentialMaterialError> {
        let bytes = STANDARD
            .decode(&self.key_base64)
            .map_err(|_| CredentialMaterialError::Invalid)?;
        ChaCha20Poly1305::new_from_slice(&bytes).map_err(|_| CredentialMaterialError::Invalid)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedPayload {
    key_id: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// One explicit transport installation, shared by issuance and exact resolution.
/// The file contains only transport keys, never Provider keys or Control seal keys.
pub struct RecipientKeyEnvelope {
    path: PathBuf,
    capabilities: BTreeSet<CredentialMaterialCapability>,
}

impl RecipientKeyEnvelope {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CredentialMaterialError> {
        let path = path.into();
        let document = Self::read_document(&path)?;
        let capabilities = document
            .keys
            .iter()
            .map(|key| CredentialMaterialCapability {
                holder: key.holder.clone(),
                source: CredentialMaterialSource::ControlPlaneReference,
                envelope: true,
            })
            .collect();
        Ok(Self { path, capabilities })
    }

    fn read_document(path: &Path) -> Result<KeyDocument, CredentialMaterialError> {
        let file = std::fs::File::open(path).map_err(|_| CredentialMaterialError::Unavailable)?;
        let metadata = file
            .metadata()
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        if !metadata.is_file() {
            return Err(CredentialMaterialError::Invalid);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(CredentialMaterialError::Invalid);
            }
        }
        let mut bytes = Vec::new();
        file.take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        if bytes.len() > MAX_BYTES {
            return Err(CredentialMaterialError::Invalid);
        }
        let document: KeyDocument =
            serde_json::from_slice(&bytes).map_err(|_| CredentialMaterialError::Invalid)?;
        if document.ttl_seconds == 0
            || document.ttl_seconds > MAX_TTL_SECONDS
            || document.keys.is_empty()
        {
            return Err(CredentialMaterialError::Invalid);
        }
        let mut ids = BTreeSet::new();
        let mut holders = BTreeSet::new();
        let mut key_bytes = BTreeSet::new();
        for key in &document.keys {
            if key.id.trim().is_empty()
                || key.holder.trust_domain.0.trim().is_empty()
                || key.holder.boundary == PlaintextBoundary::Platform
                || !ids.insert(&key.id)
                || !holders.insert(&key.holder)
                || !key_bytes.insert(
                    STANDARD
                        .decode(&key.key_base64)
                        .map_err(|_| CredentialMaterialError::Invalid)?,
                )
            {
                return Err(CredentialMaterialError::Invalid);
            }
            key.cipher()?;
        }
        Ok(document)
    }

    fn validate_access(
        access: &CredentialAccess,
        holder: &PlaintextHolder,
        binding: &CredentialMaterialBinding,
    ) -> Result<(), CredentialMaterialError> {
        binding.validate()?;
        if access.material_source != CredentialMaterialSource::ControlPlaneReference
            || !access.policy.allowed_plaintext_holders.contains(holder)
            || holder.boundary == PlaintextBoundary::Platform
        {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        // Initial self-hosted delivery qualifies scalar Provider and process
        // credentials. Structured and OAuth bundles keep their existing owners;
        // this adapter must never reinterpret either as a scalar API key.
        if access.refresh.is_some() || !access.usage.is_scalar_process_delivery() {
            return Err(CredentialMaterialError::MaterialKindMismatch);
        }
        let realization = match holder.boundary {
            PlaintextBoundary::Worker => {
                awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter
            }
            PlaintextBoundary::Workload => {
                awaken_credential_contract::CredentialRealizationKind::ProcessSecretEnvironment
            }
            PlaintextBoundary::Platform => return Err(CredentialMaterialError::RecipientMismatch),
        };
        let capabilities = awaken_credential_contract::CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [realization].into_iter().collect(),
            recipient_bound_envelopes: true,
            ..Default::default()
        };
        // Reuse the contract's complete admission chain, including durable
        // legacy-Direct rejection; the adapter must not recreate a weaker one.
        access
            .admit(
                holder,
                realization,
                &capabilities,
                super::admission::unix_time_ms(),
            )
            .map(|_| ())
            .map_err(|_| CredentialMaterialError::Invalid)
    }

    fn aad(
        access: &CredentialAccess,
        holder: &PlaintextHolder,
        binding: &CredentialMaterialBinding,
        key_id: &str,
        expires: u64,
    ) -> Result<Vec<u8>, CredentialMaterialError> {
        // Authenticate the complete claims, including expiry and key identity.
        // The canonical public fingerprint remains the interoperability check;
        // it is not treated as a cryptographic hash or used alone as AEAD data.
        serde_json::to_vec(&(
            PREFIX,
            &access.credential,
            access.material_source,
            &access.target,
            &access.usage,
            &access.policy,
            holder,
            binding,
            key_id,
            expires,
        ))
        .map_err(|_| CredentialMaterialError::Invalid)
    }

    async fn issue_exact(
        &self,
        request: CredentialEnvelopeIssuance,
    ) -> Result<CredentialEnvelope, CredentialMaterialError> {
        Self::validate_access(&request.access, &request.selected_holder, &request.binding)?;
        if request.access.envelope.is_some() || request.material.expose_secret().len() > MAX_BYTES {
            return Err(CredentialMaterialError::Invalid);
        }
        let document = Self::read_document(&self.path)?;
        let key = document
            .keys
            .iter()
            .find(|key| key.holder == request.selected_holder)
            .ok_or(CredentialMaterialError::RecipientMismatch)?;
        let expires = super::admission::unix_time_ms()
            .checked_add(document.ttl_seconds * 1000)
            .ok_or(CredentialMaterialError::Invalid)?;
        let aad = Self::aad(
            &request.access,
            &request.selected_holder,
            &request.binding,
            &key.id,
            expires,
        )?;
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = key
            .cipher()?
            .encrypt(
                &nonce,
                Payload {
                    msg: request.material.expose_secret().as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| CredentialMaterialError::Invalid)?;
        let payload = serde_json::to_vec(&SealedPayload {
            key_id: key.id.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        })
        .map_err(|_| CredentialMaterialError::Invalid)?;
        let envelope_ref = SealedCredentialEnvelopeRef {
            id: format!("{PREFIX}{}", STANDARD.encode(payload)),
            payload_fingerprint: credential_envelope_payload_fingerprint(
                &request.access,
                &request.selected_holder,
                &request.binding,
            ),
        };
        Ok(match request.selected_holder.boundary {
            PlaintextBoundary::Worker => CredentialEnvelope::SealedForWorker {
                envelope_ref,
                recipient: request.selected_holder.trust_domain,
                expires_at_unix_ms: expires,
            },
            PlaintextBoundary::Workload => CredentialEnvelope::SealedForWorkload {
                envelope_ref,
                recipient: request.selected_holder.trust_domain,
                expires_at_unix_ms: expires,
            },
            PlaintextBoundary::Platform => return Err(CredentialMaterialError::RecipientMismatch),
        })
    }
}

#[async_trait::async_trait]
impl CredentialEnvelopeIssuer for RecipientKeyEnvelope {
    async fn issue(
        &self,
        request: CredentialEnvelopeIssuance,
    ) -> Result<CredentialEnvelope, String> {
        self.issue_exact(request)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl CredentialMaterialResolver for RecipientKeyEnvelope {
    fn material_capabilities(&self) -> BTreeSet<CredentialMaterialCapability> {
        self.capabilities.clone()
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        Self::validate_access(request.access, request.selected_holder, request.binding)?;
        if !self.capabilities.contains(&CredentialMaterialCapability {
            holder: request.selected_holder.clone(),
            source: request.access.material_source,
            envelope: true,
        }) {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        let envelope = request
            .access
            .envelope
            .as_ref()
            .ok_or(CredentialMaterialError::Unavailable)?;
        envelope.validate_for_holder(request.selected_holder, super::admission::unix_time_ms())?;
        envelope.validate_issuance(request.access, request.selected_holder, request.binding)?;
        let (envelope_ref, expires) = match envelope {
            CredentialEnvelope::SealedForWorker {
                envelope_ref,
                expires_at_unix_ms,
                ..
            }
            | CredentialEnvelope::SealedForWorkload {
                envelope_ref,
                expires_at_unix_ms,
                ..
            } => (envelope_ref, *expires_at_unix_ms),
        };
        if envelope_ref.id.len() > MAX_BYTES * 8 {
            return Err(CredentialMaterialError::Invalid);
        }
        let encoded = envelope_ref
            .id
            .strip_prefix(PREFIX)
            .ok_or(CredentialMaterialError::Invalid)?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| CredentialMaterialError::Invalid)?;
        let payload: SealedPayload =
            serde_json::from_slice(&bytes).map_err(|_| CredentialMaterialError::Invalid)?;
        if payload.nonce.len() != 12 {
            return Err(CredentialMaterialError::Invalid);
        }
        let document = Self::read_document(&self.path)?;
        let key = document
            .keys
            .iter()
            .find(|key| key.id == payload.key_id && &key.holder == request.selected_holder)
            .ok_or(CredentialMaterialError::Unavailable)?;
        let aad = Self::aad(
            request.access,
            request.selected_holder,
            request.binding,
            &key.id,
            expires,
        )?;
        let plaintext = key
            .cipher()?
            .decrypt(
                Nonce::from_slice(&payload.nonce),
                Payload {
                    msg: &payload.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| CredentialMaterialError::PayloadMismatch)?;
        let secret = String::from_utf8(plaintext).map_err(|_| CredentialMaterialError::Invalid)?;
        let material = CredentialMaterial::secret(RedactedString::new(secret));
        material.validate_usage(&request.access.usage)?;
        Ok(ResolvedCredentialMaterial {
            credential: request.access.credential.clone(),
            holder: request.selected_holder.clone(),
            material,
        })
    }
}

#[cfg(test)]
mod tests;
