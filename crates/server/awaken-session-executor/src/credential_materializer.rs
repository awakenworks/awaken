//! Exact credential realization for publication-pinned runtime access.
//!
//! This host adapter owns no selection policy and cannot enumerate credentials. It
//! accepts one immutable [`ResolvedModelCandidate`], verifies its
//! Workspace/revision/usage pins against the persisted row, then materializes that exact secret. Native
//! provider execution and ACP provisioning share this adapter so they cannot drift.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{
    CredentialSource, CredentialSourceId, CredentialStatus, SecretStore,
};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_contract::{
    AttemptCredentialBinding, CredentialAccess, CredentialAdmissionError, CredentialMaterialError,
    CredentialMaterialResolver, CredentialMaterialSource, CredentialRealizationCapabilities,
    CredentialRealizationKind, CredentialUsage, PlaintextHolder, ResolvedCredentialMaterial,
};

const PROCESS_SECRET_REFERENCE_PREFIX: &str = "awaken-process-secret://";
const PROCESS_SECRET_TTL_MS: u64 = 60_000;

#[derive(Clone)]
struct PendingProcessSecret {
    candidate: ResolvedModelCandidate,
    context: awaken_runtime_contract::RuntimeRunContext,
    expires_at_unix_ms: u64,
}

/// Worker/host-side realization of one already-published credential reference.
#[derive(Clone)]
pub struct PinnedCredentialMaterializer {
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    pending_process_secrets: Arc<Mutex<HashMap<String, PendingProcessSecret>>>,
}

impl PinnedCredentialMaterializer {
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            credentials,
            secrets,
            pending_process_secrets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn claimed_provider_binding<'a>(
        candidate: &'a ResolvedModelCandidate,
        context: &'a awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
    ) -> Result<Option<(&'a CredentialAccess, &'a AttemptCredentialBinding)>, String> {
        let ModelProvisioning::Provider { credential, .. } = &candidate.provisioning else {
            return Err("model candidate has no provider provisioning".into());
        };
        let Some(access) = credential else {
            return Ok(None);
        };
        let realization = context
            .credential_realization
            .as_ref()
            .ok_or_else(|| "credential-bearing provider has no claim binding".to_string())?;
        let binding = realization
            .binding_for(candidate)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "provider candidate has no exact claim binding".to_string())?;
        if binding.credential != access.credential {
            return Err("provider claim binding selects a different credential".into());
        }
        if binding.selected_realization_kind != expected_kind {
            return Err(format!(
                "provider claim binding selects {:?}, expected {:?}",
                binding.selected_realization_kind, expected_kind
            ));
        }
        Ok(Some((access, binding)))
    }

    /// Issue a short-lived, one-shot process-secret reference for the exact
    /// credential decision already frozen in the current claim. No material is
    /// opened here; the provider's `SecretBroker` call performs ownership,
    /// revision, policy, material, and receipt checks immediately before spawn.
    pub fn plan_claimed_process_secret(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<String>, String> {
        if Self::claimed_provider_binding(
            candidate,
            context,
            CredentialRealizationKind::ProcessSecretEnvironment,
        )?
        .is_none()
        {
            return Ok(None);
        }
        let now = unix_time_ms();
        let mut pending = self
            .pending_process_secrets
            .lock()
            .map_err(|_| "process-secret registry lock is poisoned".to_string())?;
        pending.retain(|_, requirement| requirement.expires_at_unix_ms > now);
        let reference = format!("{PROCESS_SECRET_REFERENCE_PREFIX}{}", uuid::Uuid::new_v4());
        pending.insert(
            reference.clone(),
            PendingProcessSecret {
                candidate: candidate.clone(),
                context: context.clone(),
                expires_at_unix_ms: now.saturating_add(PROCESS_SECRET_TTL_MS),
            },
        );
        Ok(Some(reference))
    }

    /// Materialize the provider credential selected by the current durable claim.
    ///
    /// This is the sole inference/ACP realization path: exact binding, mechanism,
    /// ownership, secret access, and durable receipt are one ordered fail-closed
    /// operation. Callers receive no plaintext unless every cause succeeds.
    pub async fn materialize_claimed_provider(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
    ) -> Result<Option<RedactedString>, String> {
        let Some((_access, binding)) =
            Self::claimed_provider_binding(candidate, context, expected_kind)?
        else {
            return Ok(None);
        };
        let realization = context
            .credential_realization
            .as_ref()
            .expect("claimed_provider_binding requires realization");
        context
            .ownership
            .as_ref()
            .ok_or_else(|| "credential-bearing provider has no claim fence".to_string())?
            .verify_current()
            .await
            .map_err(|error| error.to_string())?;
        let secret = self
            .materialize_provider_for(
                candidate,
                &binding.selected_plaintext_holder,
                binding.selected_realization_kind,
            )
            .await?;
        realization
            .record(binding)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(secret))
    }

    /// Materialize exactly the provider credential frozen in `access`.
    ///
    /// Catalog lookup, default selection and failover are intentionally absent.
    /// Every mismatch is terminal for this pin; the caller may only try another
    /// complete candidate that was already included in the published snapshot.
    #[cfg(test)]
    async fn materialize_provider(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Result<RedactedString, String> {
        self.materialize_provider_for(
            candidate,
            &PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
            CredentialRealizationKind::WorkerProviderAdapter,
        )
        .await
    }

    /// Materialize for the exact holder and mechanism selected by admission.
    async fn materialize_provider_for(
        &self,
        candidate: &ResolvedModelCandidate,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
    ) -> Result<RedactedString, String> {
        let ModelProvisioning::Provider {
            provider_ref,
            scope_id,
            credential,
            ..
        } = &candidate.provisioning
        else {
            return Err("model candidate does not require a provider credential".into());
        };
        let provider = provider_ref
            .split_once('@')
            .map(|(provider, _)| provider)
            .ok_or_else(|| "published model candidate has no versioned provider pin".to_string())?;
        let credential = credential
            .as_ref()
            .ok_or_else(|| "published model candidate has no credential pin".to_string())?;
        if credential.usage != CredentialUsage::ProviderAdapter {
            return Err("published credential injection contract is invalid".to_string());
        }
        admit_exact_adapter(credential, selected_holder, realization)
            .map_err(|error| error.to_string())?;
        self.resolve_validated(
            credential,
            selected_holder,
            Some(scope_id.as_str()),
            Some(provider),
        )
        .await
        .map(|resolved| resolved.material)
        .map_err(|error| error.to_string())
    }

    async fn resolve_validated(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        expected_workspace: Option<&str>,
        expected_provider: Option<&str>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        if access.material_source != CredentialMaterialSource::ControlPlaneReference
            || access.envelope.is_some()
        {
            return Err(CredentialMaterialError::Unavailable);
        }
        if !access
            .policy
            .allowed_plaintext_holders
            .contains(selected_holder)
        {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        let source = self.load_active_source(&access.credential.id).await?;
        let revision =
            u64::try_from(source.version).map_err(|_| CredentialMaterialError::RevisionMismatch)?;
        if revision != access.credential.revision {
            return Err(CredentialMaterialError::RevisionMismatch);
        }
        if expected_workspace.is_some_and(|workspace| source.workspace_id != workspace) {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        if expected_provider.is_some_and(|provider| {
            source
                .provider_id
                .as_deref()
                .is_some_and(|configured| configured != provider)
        }) {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        let material = self.materialize_source(&source).await?;
        Ok(ResolvedCredentialMaterial {
            credential: access.credential.clone(),
            holder: selected_holder.clone(),
            material,
        })
    }

    pub(crate) async fn resolve_for_workspace(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
        workspace: &str,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        admit_exact_adapter(access, selected_holder, realization)
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        self.resolve_validated(access, selected_holder, Some(workspace), None)
            .await
    }

    async fn load_active_source(
        &self,
        reference: &str,
    ) -> Result<CredentialSource, CredentialMaterialError> {
        let source = self
            .credentials
            .get(&CredentialSourceId(reference.to_string()))
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        if source.status != CredentialStatus::Active {
            return Err(CredentialMaterialError::Unavailable);
        }
        Ok(source)
    }

    async fn materialize_source(
        &self,
        source: &CredentialSource,
    ) -> Result<RedactedString, CredentialMaterialError> {
        awaken_credential_vault::materialize(source, self.secrets.as_ref())
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)
    }

    pub(crate) fn secret_store(&self) -> Arc<dyn SecretStore> {
        self.secrets.clone()
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Validate one explicit adapter mechanism without introducing a capability
/// search. The caller is the installed adapter; this function only prevents its
/// Native, ACP, MCP, and Resource entry points from drifting in admission rules.
fn admit_exact_adapter(
    access: &CredentialAccess,
    selected_holder: &PlaintextHolder,
    realization: CredentialRealizationKind,
) -> Result<(), CredentialAdmissionError> {
    access.admit(
        selected_holder,
        realization,
        &CredentialRealizationCapabilities {
            holders: [selected_holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [realization].into_iter().collect(),
        },
        unix_time_ms(),
    )?;
    Ok(())
}

#[async_trait::async_trait]
impl CredentialMaterialResolver for PinnedCredentialMaterializer {
    async fn resolve_exact(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        self.resolve_validated(access, selected_holder, None, None)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SecretBroker for PinnedCredentialMaterializer {
    async fn materialize(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        let source = self
            .load_active_source(reference)
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        self.materialize_source(&source)
            .await
            .map(|secret| secret.expose_secret().as_bytes().to_vec())
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
    }

    async fn materialize_process(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        if !reference.starts_with(PROCESS_SECRET_REFERENCE_PREFIX) {
            return Err(awaken_provisioning_contract::SandboxError::new(
                "process-secret reference is not a claim-fenced capability",
            ));
        }
        let pending = self
            .pending_process_secrets
            .lock()
            .map_err(|_| {
                awaken_provisioning_contract::SandboxError::new(
                    "process-secret registry lock is poisoned",
                )
            })?
            .remove(reference)
            .ok_or_else(|| {
                awaken_provisioning_contract::SandboxError::new(
                    "process-secret reference is missing, expired, or already consumed",
                )
            })?;
        if pending.expires_at_unix_ms <= unix_time_ms() {
            return Err(awaken_provisioning_contract::SandboxError::new(
                "process-secret reference expired",
            ));
        }
        self.materialize_claimed_provider(
            &pending.candidate,
            &pending.context,
            CredentialRealizationKind::ProcessSecretEnvironment,
        )
        .await
        .and_then(|secret| {
            secret.ok_or_else(|| "process-secret requirement has no material".to_string())
        })
        .map(|secret| secret.expose_secret().as_bytes().to_vec())
        .map_err(awaken_provisioning_contract::SandboxError::new)
    }

    async fn write_back(
        &self,
        reference: &str,
        bytes: Vec<u8>,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let source = self
            .load_active_source(reference)
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        let material_ref = source.material_ref.as_ref().ok_or_else(|| {
            awaken_provisioning_contract::SandboxError::new(format!(
                "credential {} has no material",
                source.id.0
            ))
        })?;
        let material = String::from_utf8(bytes).map_err(|_| {
            awaken_provisioning_contract::SandboxError::new(
                "credential write-back is not valid UTF-8",
            )
        })?;
        self.secrets
            .put(material_ref, RedactedString::new(material))
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_provisioning_contract::SecretBroker;
    use awaken_runtime_contract::{
        CredentialEnvelope, CredentialExecutionPolicy, CredentialRef, CredentialUsage,
        InferenceEndpoint, ModelBinding, ModelExposurePolicy, PlaintextBoundary,
        SealedCredentialEnvelopeRef,
    };

    #[derive(Clone)]
    struct ResolutionRule {
        id: &'static str,
        control_source: bool,
        no_envelope: bool,
        holder_allowed: bool,
        source_exists: bool,
        source_active: bool,
        revision_exact: bool,
        workspace_exact: bool,
        provider_exact: bool,
        material_available: bool,
        expected: Result<(), CredentialMaterialError>,
    }

    fn selected_holder() -> PlaintextHolder {
        PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        )
    }

    /// Cause-effect graph:
    ///
    /// C0 control-plane material source -> C1 no sealed envelope
    ///  -> C2 selected holder authorized -> C3 source exists -> C4 source active
    ///  -> C5 exact revision -> C6 exact Workspace -> C7 exact provider
    ///  -> C8 material opens -> E1 exact material returned.
    ///
    /// A failed cause terminates resolution with its stable, secret-free E2
    /// error. The table uses `T` for a satisfied cause, `F` for the one failed
    /// cause, and `-` for causes that are not evaluated.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | Result |
    /// |---|---|---|---|---|---|---|---|---|---|---|
    /// | R1 | T | T | T | T | T | T | T | T | T | material |
    /// | R2 | F | - | - | - | - | - | - | - | - | unavailable |
    /// | R3 | T | F | - | - | - | - | - | - | - | unavailable |
    /// | R4 | T | T | F | - | - | - | - | - | - | recipient mismatch |
    /// | R5 | T | T | T | F | - | - | - | - | - | unavailable |
    /// | R6 | T | T | T | T | F | - | - | - | - | unavailable |
    /// | R7 | T | T | T | T | T | F | - | - | - | revision mismatch |
    /// | R8 | T | T | T | T | T | T | F | - | - | recipient mismatch |
    /// | R9 | T | T | T | T | T | T | T | F | - | recipient mismatch |
    /// | R10 | T | T | T | T | T | T | T | T | F | unavailable |
    #[tokio::test]
    async fn exact_resolution_tests_are_generated_from_the_decision_table() {
        let valid = ResolutionRule {
            id: "R1",
            control_source: true,
            no_envelope: true,
            holder_allowed: true,
            source_exists: true,
            source_active: true,
            revision_exact: true,
            workspace_exact: true,
            provider_exact: true,
            material_available: true,
            expected: Ok(()),
        };
        let rules = [
            valid.clone(),
            ResolutionRule {
                id: "R2",
                control_source: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R3",
                no_envelope: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R4",
                holder_allowed: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R5",
                source_exists: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R6",
                source_active: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R7",
                revision_exact: false,
                expected: Err(CredentialMaterialError::RevisionMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R8",
                workspace_exact: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R9",
                provider_exact: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R10",
                material_available: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid
            },
        ];

        for rule in rules {
            let credentials = Arc::new(InMemoryCredentialRepo::new());
            let secrets = Arc::new(InMemorySecretStore::new());
            let mut credential_id = "missing-credential".to_string();
            if rule.source_exists {
                let mut source = enter_credential(
                    CredentialCreateParams {
                        workspace_id: if rule.workspace_exact {
                            "workspace-a".into()
                        } else {
                            "workspace-b".into()
                        },
                        kind: CredentialKind::Vault,
                        provider_id: Some(if rule.provider_exact {
                            "anthropic".into()
                        } else {
                            "other-provider".into()
                        }),
                        env_key: None,
                        secret: Some(RedactedString::new("decision-table-secret")),
                        oauth_command: None,
                    },
                    secrets.as_ref(),
                    credentials.as_ref(),
                )
                .await
                .expect("fixture credential");
                credential_id.clone_from(&source.id.0);
                if !rule.source_active {
                    source.status = CredentialStatus::Disabled;
                }
                if !rule.material_available {
                    source.material_ref = None;
                }
                credentials.put(source).await.expect("fixture row");
            }

            let holder = selected_holder();
            let policy_holder = if rule.holder_allowed {
                holder.clone()
            } else {
                PlaintextHolder::new(PlaintextBoundary::Platform, "another-domain")
            };
            let mut access = CredentialAccess::new(
                CredentialRef {
                    id: credential_id,
                    revision: if rule.revision_exact { 1 } else { 2 },
                },
                if rule.control_source {
                    CredentialMaterialSource::ControlPlaneReference
                } else {
                    CredentialMaterialSource::WorkerReference
                },
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(policy_holder, ModelExposurePolicy::Forbidden),
            );
            if !rule.no_envelope {
                access = access.with_envelope(CredentialEnvelope::SealedForWorker {
                    envelope_ref: SealedCredentialEnvelopeRef {
                        id: "sealed-1".into(),
                        payload_fingerprint: "fp".into(),
                    },
                    recipient: holder.trust_domain.clone(),
                    expires_at_unix_ms: u64::MAX,
                });
            }
            let resolver = PinnedCredentialMaterializer::new(credentials, secrets);
            let result = resolver
                .resolve_validated(&access, &holder, Some("workspace-a"), Some("anthropic"))
                .await;
            assert!(
                !format!("{result:?}").contains("decision-table-secret"),
                "{} keeps errors and debug output secret-free",
                rule.id
            );
            match (rule.expected, result) {
                (Ok(()), Ok(material)) => {
                    assert_eq!(material.credential, access.credential, "{}", rule.id);
                    assert_eq!(material.holder, holder, "{}", rule.id);
                    assert_eq!(material.material.expose_secret(), "decision-table-secret");
                }
                (Err(expected), Err(actual)) => assert_eq!(actual, expected, "{}", rule.id),
                (expected, actual) => panic!(
                    "{}: expected {expected:?}, got {:?}",
                    rule.id,
                    actual.map(|_| ())
                ),
            }
        }
    }

    /// Adapter-admission cause graph: published source -> installed source
    /// capability -> material lookup. The decision table distinguishes a
    /// supported-but-missing Control reference from a Worker reference this
    /// adapter must reject before any lookup.
    ///
    /// | Rule | Control reference | Row exists | Result |
    /// |---|---|---|---|
    /// | A1 | T | F | material unavailable |
    /// | A2 | F | - | material source unsupported |
    #[tokio::test]
    async fn provider_adapter_capabilities_match_the_decision_table() {
        for (rule, source, expected) in [
            (
                "A1",
                CredentialMaterialSource::ControlPlaneReference,
                "credential material unavailable",
            ),
            (
                "A2",
                CredentialMaterialSource::WorkerReference,
                "credential material source is unsupported",
            ),
        ] {
            let candidate = ResolvedModelCandidate::provider(
                ModelBinding::new("anthropic", "model", "native"),
                "anthropic@1",
                "endpoint@1",
                "workspace-a",
                Some(CredentialAccess::new(
                    CredentialRef {
                        id: "missing".into(),
                        revision: 1,
                    },
                    source,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::self_hosted_provider(),
                )),
                InferenceEndpoint {
                    adapter_kind: "anthropic".into(),
                    base_url: "https://example.invalid".into(),
                    upstream_model: "model".into(),
                },
            );
            let materializer = PinnedCredentialMaterializer::new(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
            );
            assert_eq!(
                materializer
                    .materialize_provider(&candidate)
                    .await
                    .unwrap_err(),
                expected,
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn credential_file_broker_reuses_the_persisted_vault() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("tool".into()),
                env_key: None,
                secret: Some(RedactedString::new("before")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let broker = PinnedCredentialMaterializer::new(credentials, secrets);

        assert_eq!(broker.materialize(&source.id.0).await.unwrap(), b"before");
        assert!(
            broker.materialize_process(&source.id.0).await.is_err(),
            "a durable file credential id is never a process capability"
        );
        broker
            .write_back(&source.id.0, b"after".to_vec())
            .await
            .unwrap();
        assert_eq!(broker.materialize(&source.id.0).await.unwrap(), b"after");
    }
}
