//! Pure source-to-access compiler shared by protocol and hosted compositions.
//!
//! The compiler owns no row lookup, material read, envelope issuance, or
//! selection. Callers supply one already-selected source and retain their
//! existing I/O/custody responsibilities.

use awaken_credential_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding,
    CredentialMaterialSource, CredentialRef, CredentialTarget, CredentialUsage, PlaintextHolder,
};

use crate::{CredentialError, CredentialMaterialOrigin, CredentialSource, CredentialStatus};

pub struct ExactCredentialAccessRequest<'a> {
    pub workspace_id: Option<&'a str>,
    pub target: Option<CredentialTarget>,
    pub usage: CredentialUsage,
    pub policy: CredentialExecutionPolicy,
    /// Exact execution boundary selected by the trusted Environment/deployment
    /// profile before publication. Runtime failure can never search the policy
    /// set or switch to another trust domain.
    pub selected_holder: &'a PlaintextHolder,
    pub binding: &'a CredentialMaterialBinding,
    pub now_unix_ms: u64,
}

pub fn compile_exact_credential_access(
    source: &CredentialSource,
    request: ExactCredentialAccessRequest<'_>,
) -> Result<CredentialAccess, CredentialError> {
    let ExactCredentialAccessRequest {
        workspace_id,
        target,
        usage,
        policy,
        selected_holder,
        binding,
        now_unix_ms,
    } = request;
    source.validate_authority()?;
    if source.status != CredentialStatus::Active {
        return Err(CredentialError::NotActive(source.id.0.clone()));
    }
    if workspace_id.is_some_and(|workspace_id| source.workspace_id != workspace_id) {
        return Err(CredentialError::InvalidSource(
            "credential source belongs to another Workspace".into(),
        ));
    }
    binding
        .validate()
        .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
    if workspace_id.is_some_and(|workspace_id| binding.workspace_id != workspace_id) {
        return Err(CredentialError::InvalidSource(
            "credential material binding belongs to another Workspace".into(),
        ));
    }
    if policy.allowed_plaintext_holders.is_empty() {
        return Err(CredentialError::InvalidSource(
            "credential policy has no allowed plaintext holder".into(),
        ));
    }
    if !policy.allowed_plaintext_holders.contains(selected_holder) {
        return Err(CredentialError::InvalidSource(
            "selected plaintext holder is not authorized by credential policy".into(),
        ));
    }
    source.validate_access_target(target.as_ref(), &usage)?;
    if let (Some(descriptor), Some(_)) = (&source.descriptor, target.as_ref()) {
        descriptor
            .validate_expiry(now_unix_ms)
            .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
    }
    let material_source = match source.material_origin() {
        CredentialMaterialOrigin::Vault | CredentialMaterialOrigin::ExternalHelper => {
            CredentialMaterialSource::ControlPlaneReference
        }
        CredentialMaterialOrigin::WorkerLocal => CredentialMaterialSource::WorkerReference,
        CredentialMaterialOrigin::LegacyEnvironment => {
            return Err(CredentialError::InvalidSource(
                "legacy environment credential sources cannot compile executable access".into(),
            ));
        }
    };
    let revision = u64::try_from(source.version)
        .ok()
        .filter(|revision| *revision > 0)
        .ok_or_else(|| {
            CredentialError::InvalidSource("credential revision must be positive".into())
        })?;
    let mut access = CredentialAccess::new(
        CredentialRef {
            id: source.id.0.clone(),
            revision,
        },
        material_source,
        usage,
        policy,
    );
    if let Some(target) = target {
        access = access.with_target(target);
    }
    Ok(access)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use awaken_credential_contract::{
        CredentialDescriptor, CredentialExecutionPolicy, CredentialMaterialBinding,
        CredentialMaterialDescriptor, CredentialPurpose, CredentialSourceId, CredentialTarget,
        CredentialTargetContract, CredentialUsage, HTTP_BASIC_MATERIAL_TYPE, ModelExposurePolicy,
        PlaintextBoundary, PlaintextHolder, repository_transport_audience,
    };

    use super::*;
    use crate::{CredentialKind, SecretRef};

    fn fixture() -> (
        CredentialSource,
        CredentialTarget,
        CredentialExecutionPolicy,
        PlaintextHolder,
        CredentialMaterialBinding,
    ) {
        let holder = PlaintextHolder::new(
            PlaintextBoundary::Worker,
            "spiffe://example.test/workspace-a/worker",
        );
        let target = CredentialTarget::new(
            CredentialPurpose::RepositoryTransport,
            repository_transport_audience("https://github.com/awaken/example.git")
                .expect("canonical target"),
        );
        let descriptor = CredentialDescriptor::new(
            "github",
            CredentialMaterialDescriptor::structured(
                HTTP_BASIC_MATERIAL_TYPE,
                ["password", "username"],
            ),
            [CredentialTargetContract::new(
                target.clone(),
                CredentialUsage::HttpBasicAuth,
            )],
        )
        .with_expiry(2_000);
        let source = CredentialSource {
            id: CredentialSourceId("credential-a".into()),
            replacement_of: None,
            workspace_id: "workspace-a".into(),
            kind: CredentialKind::Vault,
            descriptor: Some(descriptor),
            provider_id: None,
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: Some(SecretRef("secret-a".into())),
            auxiliary_material_refs: BTreeMap::new(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 7,
        };
        let policy =
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden);
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &("repository-a", 3_u64),
            &CredentialUsage::HttpBasicAuth,
        );
        (source, target, policy, holder, binding)
    }

    /// Source-to-access cause/effect graph: C1 source authority/status/revision
    /// are valid; C2 source and binding belong to the requested Workspace; C3
    /// target+usage are exactly declared and unexpired; C4 holder is authorized.
    /// Effects: E1 one exact target-bearing access is compiled; E2 every drift
    /// fails before custody/material I/O; E3 an undescribed legacy source cannot
    /// be rebound to a caller-selected exact target.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | Effect |
    /// |---|---|---|---|---|---|
    /// | A1 | T | T | T | T | E1 |
    /// | A2 | F | - | - | - | E2 |
    /// | A3 | T | F | - | T | E2 |
    /// | A4 | T | T | F | T | E2 |
    /// | A5 | T | T | T | F | E2 |
    /// | A6 | legacy | T | caller-selected | T | E3 |
    #[test]
    fn exact_access_compiler_fails_closed_for_every_authority_drift() {
        let (source, target, policy, holder, binding) = fixture();
        let access = compile_exact_credential_access(
            &source,
            ExactCredentialAccessRequest {
                workspace_id: Some("workspace-a"),
                target: Some(target.clone()),
                usage: CredentialUsage::HttpBasicAuth,
                policy: policy.clone(),
                selected_holder: &holder,
                binding: &binding,
                now_unix_ms: 1_999,
            },
        )
        .expect("A1/E1");
        assert_eq!(access.credential.revision, 7, "A1/E1 exact revision");
        assert_eq!(access.target.as_ref(), Some(&target), "A1/E1 target");

        let mut inactive = source.clone();
        inactive.status = CredentialStatus::Disabled;
        assert!(
            compile_exact_credential_access(
                &inactive,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(target.clone()),
                    usage: CredentialUsage::HttpBasicAuth,
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "A2/E2"
        );
        for invalid_version in [0, -1] {
            let mut invalid_revision = source.clone();
            invalid_revision.version = invalid_version;
            assert!(
                compile_exact_credential_access(
                    &invalid_revision,
                    ExactCredentialAccessRequest {
                        workspace_id: Some("workspace-a"),
                        target: Some(target.clone()),
                        usage: CredentialUsage::HttpBasicAuth,
                        policy: policy.clone(),
                        selected_holder: &holder,
                        binding: &binding,
                        now_unix_ms: 1_999,
                    },
                )
                .is_err(),
                "A2/E2 invalid revision {invalid_version}"
            );
        }
        assert!(
            compile_exact_credential_access(
                &source,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-b"),
                    target: Some(target.clone()),
                    usage: CredentialUsage::HttpBasicAuth,
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "A3/E2"
        );
        let other_target = CredentialTarget::new(
            CredentialPurpose::RepositoryTransport,
            repository_transport_audience("https://git.example.test/awaken/example.git")
                .expect("other target"),
        );
        assert!(
            compile_exact_credential_access(
                &source,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(other_target),
                    usage: CredentialUsage::HttpBasicAuth,
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "A4/E2 target"
        );
        assert!(
            compile_exact_credential_access(
                &source,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(target.clone()),
                    usage: CredentialUsage::HttpBasicAuth,
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &binding,
                    now_unix_ms: 2_000,
                },
            )
            .is_err(),
            "A4/E2 expiry"
        );
        let other_holder = PlaintextHolder::new(
            PlaintextBoundary::Worker,
            "spiffe://example.test/workspace-a/other-worker",
        );
        assert!(
            compile_exact_credential_access(
                &source,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(target.clone()),
                    usage: CredentialUsage::HttpBasicAuth,
                    policy,
                    selected_holder: &other_holder,
                    binding: &binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "A5/E2"
        );

        let mut legacy = source;
        legacy.descriptor = None;
        assert!(
            compile_exact_credential_access(
                &legacy,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(target),
                    usage: CredentialUsage::HttpBasicAuth,
                    policy: CredentialExecutionPolicy::exact(
                        holder.clone(),
                        ModelExposurePolicy::Forbidden,
                    ),
                    selected_holder: &holder,
                    binding: &binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "A6/E3"
        );
    }

    /// Exact-target cause/effect graph: C1 the active positive legacy source
    /// scope owns the requested provider/origin; C2 the access carries that
    /// exact purpose/audience; C3 the selected holder is admitted. Effects: E1
    /// compile one target-bearing pin; E2 missing/wrong target, invalid revision,
    /// or unauthorized holder fails before material I/O.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | D1 | Provider | exact | T | E1 |
    /// | D2 | A2A origin | exact | T | E1 |
    /// | D3 | any | missing | T | E2 |
    /// | D4 | Provider | wrong audience | T | E2 |
    /// | D5 | Provider | exact | F | E2 |
    #[test]
    fn exact_holder_admission_is_required_for_legacy_provider_and_a2a() {
        let (described, _, _, _, _) = fixture();
        let mut legacy = described;
        legacy.descriptor = None;
        legacy.provider_id = Some("legacy-provider".into());
        let policy = CredentialExecutionPolicy::self_hosted_provider();
        let holder = policy
            .allowed_plaintext_holders
            .iter()
            .next()
            .expect("self-hosted policy has one exact holder")
            .clone();
        let provider_usage = CredentialUsage::ProviderAdapter;
        let provider_binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &("legacy-provider@1", "https://provider.example.invalid/v1"),
            &provider_usage,
        );
        let provider_target =
            CredentialTarget::new(CredentialPurpose::ProviderAdapter, "legacy-provider");
        let provider = compile_exact_credential_access(
            &legacy,
            ExactCredentialAccessRequest {
                workspace_id: Some("workspace-a"),
                target: Some(provider_target.clone()),
                usage: provider_usage.clone(),
                policy: policy.clone(),
                selected_holder: &holder,
                binding: &provider_binding,
                now_unix_ms: 1_999,
            },
        )
        .expect("D1/E1");
        assert_eq!(provider.credential.revision, 7, "D1/E1");

        let mut a2a_source = legacy.clone();
        a2a_source.provider_id = Some("https://agent.example".into());
        let a2a_usage = CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        };
        let a2a_binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &("a2a:https://agent.example/service", "sha256:card-security"),
            &a2a_usage,
        );
        let a2a_target = CredentialTarget::new(
            CredentialPurpose::RemoteAgentAuthorization,
            "https://agent.example",
        );
        assert!(
            compile_exact_credential_access(
                &a2a_source,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(a2a_target),
                    usage: a2a_usage.clone(),
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &a2a_binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_ok(),
            "D2/E1"
        );

        assert!(
            compile_exact_credential_access(
                &legacy,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: None,
                    usage: provider_usage.clone(),
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &provider_binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "D3/E2"
        );
        assert!(
            compile_exact_credential_access(
                &legacy,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(CredentialTarget::new(
                        CredentialPurpose::ProviderAdapter,
                        "another-provider",
                    )),
                    usage: provider_usage.clone(),
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &provider_binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "D4/E2"
        );
        assert!(
            compile_exact_credential_access(
                &legacy,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: Some(provider_target),
                    usage: provider_usage,
                    policy: CredentialExecutionPolicy::new(
                        std::iter::empty::<PlaintextHolder>(),
                        ModelExposurePolicy::Forbidden,
                    ),
                    selected_holder: &holder,
                    binding: &provider_binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "D5/E2"
        );
    }

    /// Material-origin cause/effect graph: C1 the local-injection source is
    /// otherwise admissible; C2 its canonical `material_origin()` is Vault,
    /// ExternalHelper, WorkerLocal, or LegacyEnvironment. Effects: E1 Vault and
    /// ExternalHelper compile a Control-plane reference; E2 WorkerLocal compiles
    /// a Worker reference; E3 LegacyEnvironment is rejected before any caller
    /// can select a material source or fallback.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | O1 | T | Vault | E1 |
    /// | O2 | T | ExternalHelper | E1 |
    /// | O3 | T | WorkerLocal | E2 |
    /// | O4 | T | LegacyEnvironment | E3 |
    #[test]
    fn exact_access_uses_the_source_owned_material_origin() {
        let (mut source, _, policy, holder, binding) = fixture();
        source.descriptor = None;
        let usage = CredentialUsage::EnvironmentVariable {
            name: "AWAKEN_TEST_KEY".into(),
        };

        for (rule, kind, expected) in [
            (
                "O1",
                CredentialKind::Vault,
                CredentialMaterialSource::ControlPlaneReference,
            ),
            (
                "O2",
                CredentialKind::Oauth,
                CredentialMaterialSource::ControlPlaneReference,
            ),
            (
                "O3",
                CredentialKind::WorkerLocal,
                CredentialMaterialSource::WorkerReference,
            ),
        ] {
            let mut candidate = source.clone();
            candidate.kind = kind;
            let access = compile_exact_credential_access(
                &candidate,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: None,
                    usage: usage.clone(),
                    policy: policy.clone(),
                    selected_holder: &holder,
                    binding: &binding,
                    now_unix_ms: 1_999,
                },
            )
            .unwrap_or_else(|error| panic!("{rule} must compile: {error}"));
            assert_eq!(access.material_source, expected, "{rule}");
        }

        source.kind = CredentialKind::Env;
        assert!(
            compile_exact_credential_access(
                &source,
                ExactCredentialAccessRequest {
                    workspace_id: Some("workspace-a"),
                    target: None,
                    usage,
                    policy,
                    selected_holder: &holder,
                    binding: &binding,
                    now_unix_ms: 1_999,
                },
            )
            .is_err(),
            "O4/E3"
        );
    }
}
