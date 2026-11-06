use super::*;
use serde_json::json;

#[test]
fn legacy_platform_policy_roundtrips_without_rewriting_snapshot_bytes() {
    let old = json!({"allowed_plaintext_holders":[{"boundary":"worker","trust_domain":"worker-a"}],"model_exposure":"forbidden"});
    let policy: CredentialExecutionPolicy = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(
        policy.handling_mode,
        CredentialHandlingMode::PlatformManaged
    );
    assert_eq!(serde_json::to_value(policy).unwrap(), old);
    assert!(serde_json::from_value::<CredentialExecutionPolicy>(json!({"handling_mode":"unknown","allowed_plaintext_holders":[],"model_exposure":"forbidden"})).is_err());
}

#[test]
fn application_handling_is_not_a_bypass_for_holders_exposure_or_native_authentication() {
    let holder = PlaintextHolder::new(PlaintextBoundary::Workload, "app-a");
    let usage = CredentialUsage::Extension {
        consumer_id: "app-a".into(),
        material_type: "app.material/v1".into(),
        public_config: json!({}),
    };
    let access = CredentialAccess::new(
        CredentialRef {
            id: "c".into(),
            revision: 1,
        },
        CredentialMaterialSource::ControlPlaneReference,
        usage,
        CredentialExecutionPolicy::application_managed(holder.clone()),
    )
    .with_target(CredentialTarget::new(
        CredentialPurpose::Extension,
        "application://app-a/account",
    ));
    let capabilities = CredentialRealizationCapabilities {
        holders: [holder.clone()].into_iter().collect(),
        material_sources: [CredentialMaterialSource::ControlPlaneReference]
            .into_iter()
            .collect(),
        realization_kinds: [CredentialRealizationKind::ProcessProtocolField]
            .into_iter()
            .collect(),
        recipient_bound_envelopes: true,
        extension_consumers: [(
            "app-a".into(),
            ["app.material/v1".into()].into_iter().collect(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    assert!(
        access
            .admit(
                &holder,
                CredentialRealizationKind::ProcessProtocolField,
                &capabilities,
                1
            )
            .is_ok()
    );
    for n in 0..4 {
        let mut bad = access.clone();
        let mut selected = holder.clone();
        match n {
            0 => bad.policy.model_exposure = ModelExposurePolicy::VirtualOnly,
            1 => selected.boundary = PlaintextBoundary::Worker,
            2 => bad.usage = CredentialUsage::ProviderAdapter,
            _ => {
                bad.usage = CredentialUsage::HttpHeader {
                    name: "Authorization".into(),
                    scheme: Some("Bearer".into()),
                }
            }
        }
        assert_eq!(
            bad.admit(
                &selected,
                CredentialRealizationKind::ProcessProtocolField,
                &capabilities,
                1
            ),
            Err(CredentialAdmissionError::InvalidHandlingPolicy)
        );
    }
    let mut uninstalled = capabilities;
    uninstalled.extension_consumers.clear();
    assert_eq!(
        access.admit(
            &holder,
            CredentialRealizationKind::ProcessProtocolField,
            &uninstalled,
            1
        ),
        Err(CredentialAdmissionError::ExtensionConsumerUnsupported)
    );
}

#[test]
fn standard_mcp_authentication_remains_platform_managed() {
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker-a");
    let mut credential = CredentialAccess::new(
        CredentialRef {
            id: "mcp".into(),
            revision: 1,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::HttpHeader {
            name: "Authorization".into(),
            scheme: Some("Bearer".into()),
        },
        CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
    );
    assert!(McpCredentialRequirement::from_attachment(Some(&credential), Some(&holder)).is_ok());
    credential.policy.handling_mode = CredentialHandlingMode::ApplicationManaged;
    assert!(McpCredentialRequirement::from_attachment(Some(&credential), Some(&holder)).is_err());
}
