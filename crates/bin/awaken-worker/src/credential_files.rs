// @formal entrypoint credential.worker_plaintext_holder_projection_is_exact_and_non_widening
// @formal evidence credential.material_profile_admission_preserves_complete_tuples
//! Exact Worker-private credential materialization from projected files.
//!
//! Kubernetes Secrets and Vault CSI drivers may project material into the
//! Worker's trust domain without giving the Worker a Control database or putting
//! plaintext in an Awaken wire value. This adapter consumes publication-pinned
//! `WorkerReference` material and recipient-bound `ControlPlaneReference`
//! envelopes; every path includes the exact revision, Workspace, and target/use
//! fingerprint, so a file cannot be replayed for another use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use awaken_runtime_contract::{
    CredentialEnvelope, CredentialMaterial, CredentialMaterialError, CredentialMaterialRequest,
    CredentialMaterialResolver, CredentialMaterialSource, CredentialUsage, PlaintextHolder,
    RedactedString, ResolvedCredentialMaterial, StructuredCredentialMaterial,
};

#[path = "worker_projection.rs"]
mod worker_projection;

const MAX_PROJECTED_FIELD_BYTES: u64 = 1024 * 1024;

/// Worker-private adapter for exact file-projected credential material.
///
/// The directory layout is:
///
/// ```text
/// <root>/<hex credential id>/<revision>/<hex workspace>/<hex target-use fingerprint>/
///   secret
///   username + password  # only for awaken.http-basic/v1
///
/// <root>/envelopes/<hex envelope id>/<hex payload fingerprint>/
///   <hex credential id>/<revision>/<hex workspace>/<hex target-use fingerprint>/
///   payload_fingerprint
///   secret
/// ```
///
/// Hex-encoding every untrusted component makes path traversal impossible while
/// preserving an operator-derivable, deterministic projection location.
pub struct WorkerCredentialFileResolver {
    root: PathBuf,
    holder: PlaintextHolder,
    /// The Worker's wall clock; a sealed envelope's validity is judged on it.
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
}

impl WorkerCredentialFileResolver {
    #[must_use]
    pub fn new(
        root: impl Into<PathBuf>,
        trust_domain: impl Into<String>,
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            root: root.into(),
            holder: worker_projection::exact_worker_holder(trust_domain),
            clock,
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
        let root = match request.access.envelope.as_ref() {
            Some(CredentialEnvelope::SealedForWorker { envelope_ref, .. }) => self
                .root
                .join("envelopes")
                .join(hex_component(&envelope_ref.id))
                .join(hex_component(&envelope_ref.payload_fingerprint)),
            Some(CredentialEnvelope::SealedForWorkload { .. }) | None => self.root.clone(),
        };
        root.join(hex_component(&request.access.credential.id))
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

    async fn verify_envelope_projection(
        directory: &Path,
        expected_fingerprint: &str,
    ) -> Result<(), CredentialMaterialError> {
        let projected = Self::read_field(directory, "payload_fingerprint").await?;
        if projected.expose_secret().trim() != expected_fingerprint {
            return Err(CredentialMaterialError::PayloadMismatch);
        }
        Ok(())
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
    fn material_capabilities(
        &self,
    ) -> BTreeSet<awaken_runtime_contract::CredentialMaterialCapability> {
        [
            (
                awaken_runtime_contract::CredentialMaterialSource::WorkerReference,
                false,
            ),
            (
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                true,
            ),
        ]
        .into_iter()
        .map(
            |(source, envelope)| awaken_runtime_contract::CredentialMaterialCapability {
                holder: self.holder.clone(),
                source,
                envelope,
            },
        )
        .collect()
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        request.binding.validate()?;
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
        match (
            request.access.material_source,
            request.access.envelope.as_ref(),
        ) {
            (CredentialMaterialSource::WorkerReference, None) => {}
            (
                CredentialMaterialSource::ControlPlaneReference,
                Some(envelope @ CredentialEnvelope::SealedForWorker { envelope_ref, .. }),
            ) => {
                envelope.validate_for_holder(
                    request.selected_holder,
                    awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
                )?;
                if envelope_ref.id.trim().is_empty()
                    || envelope_ref.payload_fingerprint.trim().is_empty()
                {
                    return Err(CredentialMaterialError::PayloadMismatch);
                }
                Self::verify_envelope_projection(&directory, &envelope_ref.payload_fingerprint)
                    .await?;
            }
            _ => return Err(CredentialMaterialError::Unavailable),
        }
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
        CredentialRef, ModelExposurePolicy, PlaintextBoundary, SealedCredentialEnvelopeRef,
        TrustDomainRef,
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
        let resolver =
            WorkerCredentialFileResolver::new(root, "worker-a", awaken_time::system_clock());
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

    #[tokio::test]
    async fn advertised_admission_matches_the_concrete_file_resolver() {
        // Preconditions: valid target, policy, revision/binding, populated
        // material, and (when present) unexpired envelope with exact marker.
        // Cross 2 resolver domains x 2 sources x 2 envelope modes x 3 holders
        // = 24 rows. R1 exact Worker + unsealed Worker reference => accept;
        // R2 exact Worker + sealed Control reference => accept; R3 all other
        // tuples => reject. Thus neither missing bytes nor another failed
        // precondition can hide an overbroad or underreported capability.
        // Both the decoded production declaration and the actual file resolver
        // must match R1/R2/R3. Comparing only the two results would let a blanket
        // rejection incorrectly pass the positive rows.
        use awaken_runtime_contract::{
            CredentialRealizationCapabilities, CredentialRealizationKind,
        };
        use std::sync::Arc;
        let mut mismatches = Vec::new();
        for domain in ["awaken.worker", "custom.worker"] {
            for source in [
                CredentialMaterialSource::ControlPlaneReference,
                CredentialMaterialSource::WorkerReference,
            ] {
                for envelope in [false, true] {
                    let root = temp_root("capability-relation");
                    let resolver = Arc::new(WorkerCredentialFileResolver::new(
                        &root,
                        domain,
                        awaken_time::system_clock(),
                    ));
                    for holder in [
                        resolver.holder().clone(),
                        PlaintextHolder::new(PlaintextBoundary::Worker, "other.worker"),
                        PlaintextHolder::new(PlaintextBoundary::Workload, "awaken.workload.acp"),
                    ] {
                        let mut access = CredentialAccess::new(
                            CredentialRef {
                                id: "source-a".into(),
                                revision: 1,
                            },
                            source,
                            CredentialUsage::ProviderAdapter,
                            CredentialExecutionPolicy::exact(
                                holder.clone(),
                                ModelExposurePolicy::Forbidden,
                            ),
                        )
                        .with_target(awaken_runtime_contract::credential::CredentialTarget::new(
                            awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                            "provider-a",
                        ));
                        if envelope {
                            access = access.with_envelope(CredentialEnvelope::SealedForWorker {
                                envelope_ref: SealedCredentialEnvelopeRef {
                                    id: "envelope-a".into(),
                                    payload_fingerprint: "payload-a".into(),
                                },
                                recipient: resolver.holder().trust_domain.clone(),
                                expires_at_unix_ms: u64::MAX,
                            });
                        }
                        let binding = CredentialMaterialBinding::for_target(
                            "workspace-a",
                            &"provider-a",
                            &access.usage,
                        );
                        let request = CredentialMaterialRequest {
                            access: &access,
                            selected_holder: &holder,
                            binding: &binding,
                            expected_provider: None,
                        };
                        let directory = resolver.material_directory(request);
                        write(&directory, "secret", b"fixture-material").await;
                        if envelope {
                            write(&directory, "payload_fingerprint", b"payload-a").await;
                        }
                        let pinned = awaken_credential_materializer::PinnedCredentialMaterializer::external_only(resolver.clone(), awaken_time::system_clock());
                        let (profile, kind) = if holder.boundary == PlaintextBoundary::Workload {
                            (
                                pinned.process_secret_capabilities(),
                                CredentialRealizationKind::ProcessSecretEnvironment,
                            )
                        } else {
                            (
                                pinned.provider_adapter_capabilities(),
                                CredentialRealizationKind::WorkerProviderAdapter,
                            )
                        };
                        let wire = profile.manifest_capability().unwrap().into_iter().collect();
                        let decoded =
                            CredentialRealizationCapabilities::from_manifest_capabilities(&wire)
                                .unwrap();
                        let admission = access.admit(&holder, kind, &decoded, 100).is_ok();
                        let resolution = resolver.resolve_exact(request).await.is_ok();
                        let expected = holder == *resolver.holder()
                            && matches!(
                                (source, envelope),
                                (CredentialMaterialSource::WorkerReference, false)
                                    | (CredentialMaterialSource::ControlPlaneReference, true)
                            );
                        if (admission, resolution) != (expected, expected) {
                            mismatches.push((
                                domain, source, envelope, holder, admission, resolution, expected,
                            ));
                        }
                    }
                    tokio::fs::remove_dir_all(root).await.unwrap();
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "declaration/realization mismatch: {mismatches:?}"
        );
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
            expected_provider: None,
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
                    expected_provider: None,
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
            expected_provider: None,
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
    /// R5 Control reference + Worker envelope + exact recipient/expiry/payload
    /// marker + binding -> return only the projected material; R6 changed payload
    /// marker -> PayloadMismatch; R7 expired or wrong-recipient envelope -> reject
    /// before secret read; R8 changed envelope id -> exact directory is absent.
    #[tokio::test]
    async fn recipient_bound_control_projection_is_exact_and_expires() {
        let root = temp_root("envelope");
        let (resolver, mut access, holder, binding) =
            fixture(&root, CredentialUsage::ProviderAdapter);
        access.material_source = CredentialMaterialSource::ControlPlaneReference;
        access = access.with_envelope(CredentialEnvelope::SealedForWorker {
            envelope_ref: SealedCredentialEnvelopeRef {
                id: "envelope-a".into(),
                payload_fingerprint: "sha256:payload-a".into(),
            },
            recipient: holder.trust_domain.clone(),
            expires_at_unix_ms: u64::MAX,
        });
        let request = CredentialMaterialRequest {
            access: &access,
            selected_holder: &holder,
            binding: &binding,
            expected_provider: None,
        };
        let directory = resolver.material_directory(request);
        write(&directory, "payload_fingerprint", b"sha256:payload-a\n").await;
        write(&directory, "secret", b"recipient-secret").await;
        assert_eq!(
            resolver
                .resolve_exact(request)
                .await
                .expect("R5 exact envelope")
                .material
                .single_secret()
                .expect("R5 scalar")
                .expose_secret(),
            "recipient-secret",
            "R5"
        );

        write(&directory, "payload_fingerprint", b"sha256:substituted").await;
        assert_eq!(
            resolver.resolve_exact(request).await.expect_err("R6"),
            CredentialMaterialError::PayloadMismatch,
            "R6"
        );
        write(&directory, "payload_fingerprint", b"sha256:payload-a").await;

        let expired = access
            .clone()
            .with_envelope(CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "envelope-a".into(),
                    payload_fingerprint: "sha256:payload-a".into(),
                },
                recipient: holder.trust_domain.clone(),
                expires_at_unix_ms: 1,
            });
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &expired,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .expect_err("R7 expired"),
            CredentialMaterialError::EnvelopeExpired,
            "R7"
        );

        let wrong_recipient = access
            .clone()
            .with_envelope(CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "envelope-a".into(),
                    payload_fingerprint: "sha256:payload-a".into(),
                },
                recipient: TrustDomainRef("worker-b".into()),
                expires_at_unix_ms: u64::MAX,
            });
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &wrong_recipient,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .expect_err("R7 recipient"),
            CredentialMaterialError::RecipientMismatch,
            "R7"
        );

        let another_envelope = access
            .clone()
            .with_envelope(CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "envelope-b".into(),
                    payload_fingerprint: "sha256:payload-a".into(),
                },
                recipient: holder.trust_domain.clone(),
                expires_at_unix_ms: u64::MAX,
            });
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &another_envelope,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .expect_err("R8 exact envelope directory"),
            CredentialMaterialError::Unavailable,
            "R8"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Cause/effect decision table:
    /// R9 unsupported source/envelope combination or another Worker trust domain
    /// -> reject before file IO; R10 malformed UTF-8 -> unavailable without secret bytes in
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
                    expected_provider: None,
                })
                .await
                .expect_err("R9 source"),
            CredentialMaterialError::Unavailable,
            "R9"
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
                    expected_provider: None,
                })
                .await
                .expect_err("R9 envelope"),
            CredentialMaterialError::Unavailable,
            "R9"
        );

        let wrong_holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker-b");
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &access,
                    selected_holder: &wrong_holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .expect_err("R9 holder"),
            CredentialMaterialError::RecipientMismatch,
            "R9"
        );

        let request = CredentialMaterialRequest {
            access: &access,
            selected_holder: &holder,
            binding: &binding,
            expected_provider: None,
        };
        write(
            &resolver.material_directory(request),
            "secret",
            &[0xff, 0xfe],
        )
        .await;
        assert_eq!(
            resolver.resolve_exact(request).await.expect_err("R10"),
            CredentialMaterialError::Unavailable,
            "R10"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
