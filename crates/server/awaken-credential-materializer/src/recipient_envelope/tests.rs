use super::*;
use awaken_credential_contract::{CredentialExecutionPolicy, CredentialRef, ModelExposurePolicy};
use serde_json::{Value, json};

fn holder(boundary: PlaintextBoundary) -> PlaintextHolder {
    PlaintextHolder::new(
        boundary,
        match boundary {
            PlaintextBoundary::Worker => "test:worker",
            PlaintextBoundary::Workload => "test:acp",
            PlaintextBoundary::Platform => "test:platform",
        },
    )
}

fn document() -> Value {
    json!({"ttl_seconds":3600,"keys":[
        {"id":"native","holder":holder(PlaintextBoundary::Worker),"key_base64":STANDARD.encode([0x31;32])},
        {"id":"acp","holder":holder(PlaintextBoundary::Workload),"key_base64":STANDARD.encode([0x32;32])}
    ]})
}

fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn request(boundary: PlaintextBoundary) -> CredentialEnvelopeIssuance {
    let selected_holder = holder(boundary);
    let usage = match boundary {
        PlaintextBoundary::Worker => CredentialUsage::ProviderAdapter,
        _ => CredentialUsage::EnvironmentVariable {
            name: "ANTHROPIC_API_KEY".into(),
        },
    };
    let binding = CredentialMaterialBinding::for_target("workspace-a", &"endpoint-a", &usage);
    CredentialEnvelopeIssuance {
        access: CredentialAccess::new(
            CredentialRef {
                id: "credential-a".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            CredentialExecutionPolicy::exact(
                selected_holder.clone(),
                ModelExposurePolicy::Forbidden,
            ),
        )
        .with_target(awaken_credential_contract::CredentialTarget::new(
            awaken_credential_contract::CredentialPurpose::ProviderAdapter,
            "test:provider-endpoint",
        )),
        selected_holder,
        binding,
        material: RedactedString::new("synthetic-provider-material"),
    }
}

async fn opened(
    adapter: &RecipientKeyEnvelope,
    access: &CredentialAccess,
    selected_holder: &PlaintextHolder,
    binding: &CredentialMaterialBinding,
) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
    adapter
        .resolve_exact(CredentialMaterialRequest {
            access,
            selected_holder,
            binding,
        })
        .await
}

fn envelope_ref_mut(access: &mut CredentialAccess) -> &mut SealedCredentialEnvelopeRef {
    match access.envelope.as_mut().unwrap() {
        CredentialEnvelope::SealedForWorker { envelope_ref, .. }
        | CredentialEnvelope::SealedForWorkload { envelope_ref, .. } => envelope_ref,
    }
}

fn expires_mut(access: &mut CredentialAccess) -> &mut u64 {
    match access.envelope.as_mut().unwrap() {
        CredentialEnvelope::SealedForWorker {
            expires_at_unix_ms, ..
        }
        | CredentialEnvelope::SealedForWorkload {
            expires_at_unix_ms, ..
        } => expires_at_unix_ms,
    }
}

/// Causes: C1 exact Worker/Native or Workload/ACP recipient; C2 Control and
/// Worker are independently constructed from separately projected key files;
/// C3 a fresh issuance or repeated use; C4 no local Vault. Effects: E1 both
/// consume the exact scalar; E2 wire contains no plaintext; E3 fresh random
/// nonces distinguish issuances while retries preserve the original access;
/// E4 installed tuples require envelopes for their exact holder. Rules:
/// D1=C1(Worker)+C2+C3+C4=>E1-E4; D2=C1(Workload)+C2+C3+C4=>E1-E4.
#[tokio::test]
async fn separate_recipient_installations_deliver_native_and_acp_without_a_vault() {
    let root = tempfile::tempdir().unwrap();
    let control = root.path().join("control.json");
    write(&control, &document());
    let issuer = RecipientKeyEnvelope::open(control).unwrap();
    for boundary in [PlaintextBoundary::Worker, PlaintextBoundary::Workload] {
        let worker = root.path().join("worker.json");
        let mut worker_document = document();
        worker_document["keys"]
            .as_array_mut()
            .unwrap()
            .retain(|key| key["holder"] == json!(holder(boundary)));
        write(&worker, &worker_document);
        let resolver = RecipientKeyEnvelope::open(&worker).unwrap();
        let req = request(boundary);
        let access = req
            .access
            .clone()
            .with_envelope(issuer.issue(request(boundary)).await.unwrap());
        let another = issuer.issue(request(boundary)).await.unwrap();
        assert_ne!(access.envelope.as_ref(), Some(&another), "E3 fresh nonce");
        assert!(
            !serde_json::to_string(&access)
                .unwrap()
                .contains("synthetic-provider-material"),
            "E2"
        );
        for _ in 0..2 {
            let result = opened(&resolver, &access, &req.selected_holder, &req.binding)
                .await
                .unwrap();
            assert_eq!(result.credential, req.access.credential);
            assert_eq!(result.holder, req.selected_holder);
            assert_eq!(
                result.material.into_secret().unwrap().expose_secret(),
                "synthetic-provider-material"
            );
        }
        assert_eq!(
            resolver.material_capabilities(),
            [CredentialMaterialCapability {
                holder: req.selected_holder,
                source: CredentialMaterialSource::ControlPlaneReference,
                envelope: true,
            }]
            .into_iter()
            .collect()
        );
    }
}

#[tokio::test]
async fn acp_provider_adapter_usage_retains_its_workload_recipient() {
    // D17 ordinary Provider API keys retain ProviderAdapter usage when the ACP
    // executor maps them into its process environment. Only the exact holder
    // changes: issuing, resolving and replay admission must all keep Workload.
    // D18 a native Worker cannot use that envelope even under a policy allowing
    // both boundaries. This covers the real publication failure's usage shape.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keys.json");
    write(&path, &document());
    let adapter = RecipientKeyEnvelope::open(path).unwrap();
    let mut req = request(PlaintextBoundary::Workload);
    req.access.usage = CredentialUsage::ProviderAdapter;
    req.access
        .policy
        .allowed_plaintext_holders
        .insert(holder(PlaintextBoundary::Worker));
    req.binding =
        CredentialMaterialBinding::for_target("workspace-a", &"endpoint-a", &req.access.usage);
    let selected_holder = req.selected_holder.clone();
    let binding = req.binding.clone();
    let access = req
        .access
        .clone()
        .with_envelope(adapter.issue(req).await.unwrap());
    assert!(
        opened(&adapter, &access, &selected_holder, &binding)
            .await
            .is_ok()
    );
    assert!(
        opened(
            &adapter,
            &access,
            &holder(PlaintextBoundary::Worker),
            &binding
        )
        .await
        .is_err()
    );
}

/// Cause/effect graph: a valid ciphertext is fixed; independently change an
/// authority claim or ciphertext component. D3 absent envelope, D4 expired,
/// D5 extended expiry, D6 revision, D7 Workspace, D8 target/use, D9 holder,
/// D10 policy, D11 ciphertext, D12 nonce, D13 key identity, D14 public payload
/// fingerprint, D15 material source, D16 usage. Every rule => reject before
/// returning plaintext. Recomputing the public fingerprint in D6-D10/D15-D16
/// constrains these rules to test AEAD/holder validation, not only a stale hash.
#[tokio::test]
async fn every_independent_replay_coordinate_and_ciphertext_mutation_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keys.json");
    write(&path, &document());
    let adapter = RecipientKeyEnvelope::open(path).unwrap();
    for boundary in [PlaintextBoundary::Worker, PlaintextBoundary::Workload] {
        let req = request(boundary);
        let original = req
            .access
            .clone()
            .with_envelope(adapter.issue(request(boundary)).await.unwrap());
        for rule in 3..=16 {
            let mut access = original.clone();
            let mut binding = req.binding.clone();
            let mut selected = req.selected_holder.clone();
            match rule {
                3 => access.envelope = None,
                4 => *expires_mut(&mut access) = 1,
                5 => *expires_mut(&mut access) += 1_000_000,
                6 => access.credential.revision += 1,
                7 => binding.workspace_id.push_str("-foreign"),
                8 => binding.target_use_fingerprint.push_str("-foreign"),
                9 => selected.trust_domain.0.push_str("-foreign"),
                10 => access.policy.model_exposure = ModelExposurePolicy::VirtualOnly,
                11..=13 => {
                    let reference = envelope_ref_mut(&mut access);
                    let bytes = STANDARD
                        .decode(reference.id.strip_prefix(PREFIX).unwrap())
                        .unwrap();
                    let mut payload: SealedPayload = serde_json::from_slice(&bytes).unwrap();
                    match rule {
                        11 => payload.ciphertext[0] ^= 1,
                        12 => payload.nonce.clear(),
                        13 => payload.key_id.push_str("-foreign"),
                        _ => unreachable!(),
                    }
                    reference.id = format!(
                        "{PREFIX}{}",
                        STANDARD.encode(serde_json::to_vec(&payload).unwrap())
                    );
                }
                14 => envelope_ref_mut(&mut access)
                    .payload_fingerprint
                    .push_str("-foreign"),
                15 => access.material_source = CredentialMaterialSource::WorkerReference,
                16 => {
                    access.usage = CredentialUsage::EnvironmentVariable {
                        name: "OTHER_KEY".into(),
                    }
                }
                _ => unreachable!(),
            }
            if matches!(rule, 6..=10 | 15..=16) {
                let fingerprint =
                    credential_envelope_payload_fingerprint(&access, &selected, &binding);
                envelope_ref_mut(&mut access).payload_fingerprint = fingerprint;
            }
            assert!(
                opened(&adapter, &access, &selected, &binding)
                    .await
                    .is_err(),
                "D{rule} {boundary:?}"
            );
        }
        assert!(
            opened(&adapter, &original, &req.selected_holder, &req.binding)
                .await
                .is_ok(),
            "denial never corrupts valid access"
        );
    }
}

/// D17 remove the exact recipient key after construction; D18 change its bytes;
/// D19 remove the file; D20 restore the original deployment. Effects: D17-D19
/// fail on every use and issuance with no cached-key/local-Vault fallback;
/// D20 resolves the original immutable publication. This proves transport-key
/// revocation only, not credential lifecycle propagation across a partition.
#[tokio::test]
async fn key_removal_and_replacement_are_observed_before_use_without_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keys.json");
    write(&path, &document());
    let adapter = RecipientKeyEnvelope::open(&path).unwrap();
    let req = request(PlaintextBoundary::Worker);
    let access = req.access.clone().with_envelope(
        adapter
            .issue(request(PlaintextBoundary::Worker))
            .await
            .unwrap(),
    );
    for rule in 17..=19 {
        let mut changed = document();
        match rule {
            17 => {
                changed["keys"].as_array_mut().unwrap().remove(0);
                write(&path, &changed);
            }
            18 => {
                changed["keys"][0]["key_base64"] = json!(STANDARD.encode([0x33; 32]));
                write(&path, &changed);
            }
            19 => std::fs::remove_file(&path).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            opened(&adapter, &access, &req.selected_holder, &req.binding)
                .await
                .is_err(),
            "D{rule}"
        );
        if rule != 18 {
            assert!(
                adapter
                    .issue(request(PlaintextBoundary::Worker))
                    .await
                    .is_err()
            );
        }
        write(&path, &document());
    }
    assert!(
        opened(&adapter, &access, &req.selected_holder, &req.binding)
            .await
            .is_ok(),
        "D20"
    );
}

/// Configuration partitions: D21 missing/empty/oversized file, invalid JSON,
/// zero/excessive TTL, no keys, repeated id/holder/key, invalid key size/base64,
/// blank identity, Platform recipient or public file permissions => startup
/// rejects. Error formatting must not echo document bytes. These are adapter
/// configuration partitions rather than combinations of runtime authority.
#[test]
fn malformed_or_ambiguous_key_documents_fail_closed_without_echoing_secrets() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keys.json");
    for rule in 0..13 {
        let mut doc = document();
        match rule {
            0 => doc["ttl_seconds"] = json!(0),
            1 => doc["ttl_seconds"] = json!(MAX_TTL_SECONDS + 1),
            2 => doc["keys"] = json!([]),
            3 => doc["keys"][1]["id"] = doc["keys"][0]["id"].clone(),
            4 => doc["keys"][1]["holder"] = doc["keys"][0]["holder"].clone(),
            5 => doc["keys"][1]["key_base64"] = doc["keys"][0]["key_base64"].clone(),
            6 => doc["keys"][0]["key_base64"] = json!(STANDARD.encode([0x31; 31])),
            7 => doc["keys"][0]["key_base64"] = json!("PRIVATE-INVALID-MATERIAL"),
            8 => doc["keys"][0]["id"] = json!(" "),
            9 => doc["keys"][0]["holder"] = json!(holder(PlaintextBoundary::Platform)),
            10 => doc["unknown"] = json!("PRIVATE-INVALID-MATERIAL"),
            11 => doc = json!(null),
            12 => doc["keys"][0]["holder"]["trust_domain"] = json!(""),
            _ => unreachable!(),
        }
        write(&path, &doc);
        let result = RecipientKeyEnvelope::open(&path);
        assert!(result.is_err(), "D21 partition {rule}");
        assert!(
            !result
                .err()
                .unwrap()
                .to_string()
                .contains("PRIVATE-INVALID-MATERIAL")
        );
    }
    for bytes in [Vec::new(), vec![b'x'; MAX_BYTES + 1], b"{not-json".to_vec()] {
        std::fs::write(&path, bytes).unwrap();
        assert!(RecipientKeyEnvelope::open(&path).is_err());
    }
    std::fs::remove_file(&path).unwrap();
    assert!(RecipientKeyEnvelope::open(&path).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        write(&path, &document());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(RecipientKeyEnvelope::open(&path).is_err());
    }
}
