//! Wire shapes of the credential reference vocabulary.
//!
//! These values are persisted inside executable snapshots, dispatch claims and
//! receipts. They moved here from `awaken-credential-contract` and
//! `awaken-runtime-contract` (ADR-0086 §7); each test pins the exact JSON the
//! value had before the move, and that the JSON decodes back to the value.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::*;

fn pins<T>(value: &T, wire: Value)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_value(value).unwrap(), wire);
    assert_eq!(&serde_json::from_value::<T>(wire).unwrap(), value);
}

fn worker() -> PlaintextHolder {
    PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN)
}

#[test]
fn scalar_reference_values_keep_their_wire_shape() {
    pins(
        &CredentialRef {
            id: "credential-a".into(),
            revision: 7,
        },
        json!({"id": "credential-a", "revision": 7}),
    );
    pins(
        &CredentialMaterialSource::ControlPlaneReference,
        json!("control_plane_reference"),
    );
    pins(
        &CredentialMaterialSource::WorkerReference,
        json!("worker_reference"),
    );
    pins(&PlaintextBoundary::Workload, json!("workload"));
    pins(&PlaintextBoundary::Worker, json!("worker"));
    pins(&PlaintextBoundary::Platform, json!("platform"));
    pins(&TrustDomainRef("domain".into()), json!("domain"));
    pins(
        &worker(),
        json!({"boundary": "worker", "trust_domain": "awaken.worker"}),
    );
    pins(&ModelExposurePolicy::Forbidden, json!("forbidden"));
    pins(&ModelExposurePolicy::VirtualOnly, json!("virtual_only"));
    pins(&TokenEndpointAuth::None, json!("none"));
    pins(
        &TokenEndpointAuth::ClientSecretBasic,
        json!("client_secret_basic"),
    );
    pins(
        &TokenEndpointAuth::ClientSecretPost,
        json!("client_secret_post"),
    );
    pins(&CredentialAudienceRef("openai".into()), json!("openai"));
    pins(
        &CandidateFingerprint("sha256:candidate".into()),
        json!("sha256:candidate"),
    );
}

#[test]
fn policy_and_envelope_values_keep_their_wire_shape() {
    pins(
        &CredentialExecutionPolicy::exact(worker(), ModelExposurePolicy::Forbidden),
        json!({
            "allowed_plaintext_holders": [{"boundary": "worker", "trust_domain": "awaken.worker"}],
            "model_exposure": "forbidden"
        }),
    );
    let envelope_ref = SealedCredentialEnvelopeRef {
        id: "envelope-a".into(),
        payload_fingerprint: "sha256:payload".into(),
    };
    pins(
        &envelope_ref,
        json!({"id": "envelope-a", "payload_fingerprint": "sha256:payload"}),
    );
    pins(
        &CredentialEnvelope::SealedForWorker {
            envelope_ref: envelope_ref.clone(),
            recipient: TrustDomainRef("awaken.worker".into()),
            expires_at_unix_ms: 5,
        },
        json!({
            "type": "sealed_for_worker",
            "envelope_ref": {"id": "envelope-a", "payload_fingerprint": "sha256:payload"},
            "recipient": "awaken.worker",
            "expires_at_unix_ms": 5
        }),
    );
    pins(
        &CredentialEnvelope::SealedForWorkload {
            envelope_ref,
            recipient: TrustDomainRef("awaken.workload.acp".into()),
            expires_at_unix_ms: 6,
        },
        json!({
            "type": "sealed_for_workload",
            "envelope_ref": {"id": "envelope-a", "payload_fingerprint": "sha256:payload"},
            "recipient": "awaken.workload.acp",
            "expires_at_unix_ms": 6
        }),
    );
}

#[test]
fn target_and_usage_values_keep_their_wire_shape() {
    pins(
        &CredentialPurpose::WebProviderAuthorization,
        json!({"type": "web_provider_authorization"}),
    );
    pins(
        &CredentialTarget::new(CredentialPurpose::ProviderAdapter, "openai"),
        json!({"purpose": {"type": "provider_adapter"}, "audience": "openai"}),
    );
    pins(
        &CredentialUsage::ProviderAdapter,
        json!({"type": "provider_adapter"}),
    );
    pins(
        &CredentialUsage::HttpHeader {
            name: "Authorization".into(),
            scheme: Some("Bearer".into()),
        },
        json!({"type": "http_header", "name": "Authorization", "scheme": "Bearer"}),
    );
    pins(
        &CredentialUsage::HttpHeader {
            name: "X-Api-Key".into(),
            scheme: None,
        },
        json!({"type": "http_header", "name": "X-Api-Key"}),
    );
    pins(
        &CredentialUsage::HttpBasicAuth,
        json!({"type": "http_basic_auth"}),
    );
    pins(
        &CredentialUsage::process_env("OPENAI_API_KEY"),
        json!({
            "type": "extension",
            "consumer_id": PROCESS_ENV_CONSUMER,
            "material_type": PROCESS_ENV_MATERIAL_TYPE,
            "public_config": {"name": "OPENAI_API_KEY"}
        }),
    );
    pins(
        &CredentialUsage::QueryParameter { name: "key".into() },
        json!({"type": "query_parameter", "name": "key"}),
    );
    pins(
        &CredentialUsage::HttpEffect {
            fields: BTreeMap::from([(
                "token".to_string(),
                BTreeSet::from([
                    HttpEffectPlacement::Header {
                        name: "authorization".into(),
                    },
                    HttpEffectPlacement::Query { name: "t".into() },
                    HttpEffectPlacement::JsonPointer {
                        pointer: "/auth/token".into(),
                    },
                ]),
            )]),
        },
        json!({
            "type": "http_effect",
            "fields": {"token": [
                {"type": "header", "name": "authorization"},
                {"type": "query", "name": "t"},
                {"type": "json_pointer", "pointer": "/auth/token"}
            ]}
        }),
    );
    pins(
        &CredentialUsage::SignatureVerification {
            fields: BTreeMap::from([(
                "key".to_string(),
                BTreeSet::from([
                    SignatureVerificationAlgorithm::HmacSha256,
                    SignatureVerificationAlgorithm::Sha256Parts,
                    SignatureVerificationAlgorithm::ConstantTime,
                ]),
            )]),
        },
        json!({
            "type": "signature_verification",
            "fields": {"key": ["hmac_sha256", "sha256_parts", "constant_time"]}
        }),
    );
    pins(
        &CredentialUsage::ClientCertificate,
        json!({"type": "client_certificate"}),
    );
}

#[test]
fn access_and_refresh_values_keep_their_wire_shape() {
    let refresh = CredentialRefreshAccess::new(
        7,
        "https://auth.example/token".into(),
        "client-a".into(),
        TokenEndpointAuth::ClientSecretBasic,
        Some("client-secret-ref".into()),
        "refresh-ref".into(),
        "access-ref".into(),
        None,
        None,
    );
    let fingerprint = refresh.configuration_fingerprint.clone();
    pins(
        &refresh,
        json!({
            "credential_revision": 7,
            "configuration_fingerprint": fingerprint,
            "token_endpoint": "https://auth.example/token",
            "client_id": "client-a",
            "token_endpoint_auth": "client_secret_basic",
            "client_secret_ref": "client-secret-ref",
            "refresh_token_ref": "refresh-ref",
            "access_token_ref": "access-ref"
        }),
    );

    let minimal = CredentialAccess::new(
        CredentialRef {
            id: "credential-a".into(),
            revision: 7,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::ProviderAdapter,
        CredentialExecutionPolicy::exact(worker(), ModelExposurePolicy::Forbidden),
    );
    let policy = json!({
        "allowed_plaintext_holders": [{"boundary": "worker", "trust_domain": "awaken.worker"}],
        "model_exposure": "forbidden"
    });
    pins(
        &minimal,
        json!({
            "credential": {"id": "credential-a", "revision": 7},
            "material_source": "control_plane_reference",
            "usage": {"type": "provider_adapter"},
            "policy": policy
        }),
    );
    let complete = minimal
        .with_target(CredentialTarget::new(
            CredentialPurpose::ProviderAdapter,
            "openai",
        ))
        .with_envelope(CredentialEnvelope::SealedForWorker {
            envelope_ref: SealedCredentialEnvelopeRef {
                id: "envelope-a".into(),
                payload_fingerprint: "sha256:payload".into(),
            },
            recipient: TrustDomainRef("awaken.worker".into()),
            expires_at_unix_ms: 5,
        })
        .with_refresh(refresh.clone());
    pins(
        &complete,
        json!({
            "credential": {"id": "credential-a", "revision": 7},
            "material_source": "control_plane_reference",
            "envelope": {
                "type": "sealed_for_worker",
                "envelope_ref": {"id": "envelope-a", "payload_fingerprint": "sha256:payload"},
                "recipient": "awaken.worker",
                "expires_at_unix_ms": 5
            },
            "target": {"purpose": {"type": "provider_adapter"}, "audience": "openai"},
            "usage": {"type": "provider_adapter"},
            "refresh": serde_json::to_value(&refresh).unwrap(),
            "policy": policy
        }),
    );
    assert!(!complete.is_legacy_direct());
}
