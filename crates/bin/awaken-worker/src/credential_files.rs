//! Exact Worker-private credential materialization from projected files.
//!
//! Kubernetes Secrets and Vault CSI drivers may project material into the
//! Worker's trust domain without giving the Worker a Control database or putting
//! plaintext in an Awaken wire value. This adapter consumes only a publication-
//! pinned `WorkerReference`; every path includes the exact revision, Workspace,
//! and target/use fingerprint, so a file cannot be replayed for another use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use awaken_runtime_contract::{
    CredentialMaterial, CredentialMaterialError, CredentialMaterialRequest,
    CredentialMaterialResolver, CredentialMaterialSource, CredentialUsage, PlaintextBoundary,
    PlaintextHolder, RedactedString, ResolvedCredentialMaterial, StructuredCredentialMaterial,
};

const MAX_PROJECTED_FIELD_BYTES: u64 = 1024 * 1024;

/// Worker-private adapter for exact file-projected credential material.
///
/// The directory layout is:
///
/// ```text
/// <root>/<hex credential id>/<revision>/<hex workspace>/<hex target-use fingerprint>/
///   secret
///   username + password  # only for awaken.http-basic/v1
/// ```
///
/// Hex-encoding every untrusted component makes path traversal impossible while
/// preserving an operator-derivable, deterministic projection location.
pub struct WorkerCredentialFileResolver {
    root: PathBuf,
    holder: PlaintextHolder,
}

impl WorkerCredentialFileResolver {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, trust_domain: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            holder: PlaintextHolder::new(PlaintextBoundary::Worker, trust_domain),
        }
    }

    #[must_use]
    pub fn holder(&self) -> &PlaintextHolder {
        &self.holder
    }

    /// Return the deterministic directory an external secret projector must
    /// populate for one exact, secret-free request.
    #[must_use]
    pub fn material_directory(&self, request: CredentialMaterialRequest<'_>) -> PathBuf {
        self.root
            .join(hex_component(&request.access.credential.id))
            .join(request.access.credential.revision.to_string())
            .join(hex_component(&request.binding.workspace_id))
            .join(hex_component(&request.binding.target_use_fingerprint))
    }

    async fn read_field(
        directory: &Path,
        name: &str,
    ) -> Result<RedactedString, CredentialMaterialError> {
        let path = directory.join(name);
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        if !metadata.is_file() || metadata.len() > MAX_PROJECTED_FIELD_BYTES {
            return Err(CredentialMaterialError::Unavailable);
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        String::from_utf8(bytes)
            .map(RedactedString::new)
            .map_err(|_| CredentialMaterialError::Unavailable)
    }
}

fn hex_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

#[async_trait::async_trait]
impl CredentialMaterialResolver for WorkerCredentialFileResolver {
    fn supported_material_sources(&self) -> BTreeSet<CredentialMaterialSource> {
        BTreeSet::from([CredentialMaterialSource::WorkerReference])
    }

    fn supports_recipient_bound_envelopes(&self) -> bool {
        false
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        request.binding.validate()?;
        if request.access.material_source != CredentialMaterialSource::WorkerReference {
            return Err(CredentialMaterialError::Unavailable);
        }
        if request.access.envelope.is_some() {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        if request.selected_holder != &self.holder
            || !request
                .access
                .policy
                .allowed_plaintext_holders
                .contains(request.selected_holder)
        {
            return Err(CredentialMaterialError::RecipientMismatch);
        }

        let directory = self.material_directory(request);
        let material = match &request.access.usage {
            CredentialUsage::HttpBasicAuth => {
                CredentialMaterial::Structured(StructuredCredentialMaterial {
                    type_id: awaken_runtime_contract::credential::HTTP_BASIC_MATERIAL_TYPE.into(),
                    fields: BTreeMap::from([
                        (
                            "username".into(),
                            Self::read_field(&directory, "username").await?,
                        ),
                        (
                            "password".into(),
                            Self::read_field(&directory, "password").await?,
                        ),
                    ]),
                })
            }
            CredentialUsage::Extension { .. } => {
                return Err(CredentialMaterialError::MaterialKindMismatch);
            }
            _ => CredentialMaterial::secret(Self::read_field(&directory, "secret").await?),
        };
        material.validate_usage(&request.access.usage)?;
        Ok(ResolvedCredentialMaterial {
            credential: request.access.credential.clone(),
            holder: request.selected_holder.clone(),
            material,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::{
        CredentialAccess, CredentialEnvelope, CredentialExecutionPolicy, CredentialMaterialBinding,
        CredentialRef, ModelExposurePolicy, SealedCredentialEnvelopeRef,
    };

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "awaken-worker-credential-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn fixture(
        root: &Path,
        usage: CredentialUsage,
    ) -> (
        WorkerCredentialFileResolver,
        CredentialAccess,
        PlaintextHolder,
        CredentialMaterialBinding,
    ) {
        let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker-a");
        let resolver = WorkerCredentialFileResolver::new(root, "worker-a");
        let access = CredentialAccess::new(
            CredentialRef {
                id: "credential/alpha".into(),
                revision: 7,
            },
            CredentialMaterialSource::WorkerReference,
            usage,
            CredentialExecutionPolicy {
                allowed_plaintext_holders: BTreeSet::from([holder.clone()]),
                model_exposure: ModelExposurePolicy::Forbidden,
            },
        );
        let binding = CredentialMaterialBinding {
            workspace_id: "workspace-a".into(),
            target_use_fingerprint: "sha256:target-a".into(),
        };
        (resolver, access, holder, binding)
    }

    async fn write(directory: &Path, name: &str, value: &[u8]) {
        tokio::fs::create_dir_all(directory)
            .await
            .expect("create projected directory");
        tokio::fs::write(directory.join(name), value)
            .await
            .expect("write projected field");
    }

    /// Cause/effect decision table:
    /// R1 exact Worker source + holder + revision + Workspace + target binding +
    /// scalar usage -> return that scalar and exact identity; R2 any changed path
    /// dimension -> no fallback or enumeration, therefore unavailable.
    #[tokio::test]
    async fn exact_scalar_projection_is_bound_to_every_request_dimension() {
        let root = temp_root("scalar");
        let (resolver, access, holder, binding) = fixture(&root, CredentialUsage::ProviderAdapter);
        let request = CredentialMaterialRequest {
            access: &access,
            selected_holder: &holder,
            binding: &binding,
        };
        write(
            &resolver.material_directory(request),
            "secret",
            b"projected-secret",
        )
        .await;

        let resolved = resolver
            .resolve_exact(request)
            .await
            .expect("R1 exact material");
        assert_eq!(resolved.credential, access.credential, "R1");
        assert_eq!(resolved.holder, holder, "R1");
        assert_eq!(
            resolved
                .material
                .single_secret()
                .expect("scalar")
                .expose_secret(),
            "projected-secret",
            "R1"
        );

        let changed = CredentialMaterialBinding {
            target_use_fingerprint: "sha256:another-target".into(),
            ..binding.clone()
        };
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &access,
                    selected_holder: &holder,
                    binding: &changed,
                })
                .await
                .expect_err("R2 must not search another binding"),
            CredentialMaterialError::Unavailable,
            "R2"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Cause/effect decision table:
    /// R3 canonical HTTP Basic usage + both exact projected fields -> typed
    /// `awaken.http-basic/v1`; R4 a missing field -> unavailable, never reinterpret
    /// one field as a scalar secret.
    #[tokio::test]
    async fn http_basic_projection_requires_both_typed_fields() {
        let root = temp_root("basic");
        let (resolver, access, holder, binding) = fixture(&root, CredentialUsage::HttpBasicAuth);
        let request = CredentialMaterialRequest {
            access: &access,
            selected_holder: &holder,
            binding: &binding,
        };
        let directory = resolver.material_directory(request);
        write(&directory, "username", b"git-user").await;
        assert_eq!(
            resolver.resolve_exact(request).await.expect_err("R4"),
            CredentialMaterialError::Unavailable,
            "R4"
        );
        write(&directory, "password", b"git-password").await;
        let CredentialMaterial::Structured(material) =
            resolver.resolve_exact(request).await.expect("R3").material
        else {
            panic!("R3 expected typed HTTP Basic material");
        };
        assert_eq!(
            material.type_id,
            awaken_runtime_contract::credential::HTTP_BASIC_MATERIAL_TYPE,
            "R3"
        );
        assert_eq!(
            material.fields["password"].expose_secret(),
            "git-password",
            "R3"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Cause/effect decision table:
    /// R5 Control source, another Worker trust domain, or an envelope -> reject
    /// before file IO; R6 malformed UTF-8 -> unavailable without secret bytes in
    /// diagnostics.
    #[tokio::test]
    async fn unsupported_source_holder_envelope_and_encoding_fail_closed() {
        let root = temp_root("reject");
        let (resolver, mut access, holder, binding) =
            fixture(&root, CredentialUsage::ProviderAdapter);
        access.material_source = CredentialMaterialSource::ControlPlaneReference;
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &access,
                    selected_holder: &holder,
                    binding: &binding,
                })
                .await
                .expect_err("R5 source"),
            CredentialMaterialError::Unavailable,
            "R5"
        );

        access.material_source = CredentialMaterialSource::WorkerReference;
        let enveloped = access
            .clone()
            .with_envelope(CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "sealed-1".into(),
                    payload_fingerprint: "sha256:sealed".into(),
                },
                recipient: holder.trust_domain.clone(),
                expires_at_unix_ms: u64::MAX,
            });
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &enveloped,
                    selected_holder: &holder,
                    binding: &binding,
                })
                .await
                .expect_err("R5 envelope"),
            CredentialMaterialError::RecipientMismatch,
            "R5"
        );

        let wrong_holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker-b");
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &access,
                    selected_holder: &wrong_holder,
                    binding: &binding,
                })
                .await
                .expect_err("R5 holder"),
            CredentialMaterialError::RecipientMismatch,
            "R5"
        );

        let request = CredentialMaterialRequest {
            access: &access,
            selected_holder: &holder,
            binding: &binding,
        };
        write(
            &resolver.material_directory(request),
            "secret",
            &[0xff, 0xfe],
        )
        .await;
        assert_eq!(
            resolver.resolve_exact(request).await.expect_err("R6"),
            CredentialMaterialError::Unavailable,
            "R6"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
