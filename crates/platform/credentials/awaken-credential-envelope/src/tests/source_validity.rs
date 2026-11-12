//! Source validity is authenticated independently of its public fingerprint.

use super::*;

/// For both recipient classes, all four optional metadata shapes round-trip.
/// Adding, changing or removing either coordinate then recomputing the public
/// fingerprint passes issuance identity checks but must fail actual AEAD open.
/// This catches a forged longer source lifetime or a different issuer binding,
/// rather than merely checking that two public hashes differ.
#[tokio::test]
async fn source_validity_tampering_rejects_after_the_public_fingerprint_is_recomputed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keys.json");
    write(&path, &document());
    let adapter = RecipientKeyEnvelope::open(path, awaken_time::system_clock()).unwrap();
    for boundary in [PlaintextBoundary::Worker, PlaintextBoundary::Workload] {
        for (expiry, generation) in [
            (None, None),
            (Some(u64::MAX - 1), None),
            (None, Some(41)),
            (Some(u64::MAX - 1), Some(41)),
        ] {
            let mut req = request(boundary);
            req.access.expires_at_unix_ms = expiry;
            req.access.issuer_generation = generation;
            let holder = req.selected_holder.clone();
            let binding = req.binding.clone();
            let access = req.access.clone();
            let original = access.with_envelope(adapter.issue(req).await.unwrap());
            assert!(opened(&adapter, &original, &holder, &binding).await.is_ok());
            for coordinate in 0..4 {
                let mut forged = original.clone();
                match coordinate {
                    0 => forged.expires_at_unix_ms = Some(u64::MAX),
                    1 => forged.issuer_generation = Some(42),
                    2 if expiry.is_some() => forged.expires_at_unix_ms = None,
                    3 if generation.is_some() => forged.issuer_generation = None,
                    _ => continue,
                }
                let fingerprint =
                    credential_envelope_payload_fingerprint(&forged, &holder, &binding);
                envelope_ref_mut(&mut forged).payload_fingerprint = fingerprint;
                forged
                    .envelope
                    .as_ref()
                    .unwrap()
                    .validate_issuance(&forged, &holder, &binding)
                    .expect("the attacker can recompute every public claim fingerprint");
                assert!(
                    matches!(
                        opened(&adapter, &forged, &holder, &binding).await,
                        Err(CredentialMaterialError::PayloadMismatch)
                    ),
                    "{boundary:?}, expiry={expiry:?}, generation={generation:?}, coordinate={coordinate}"
                );
            }
            assert!(opened(&adapter, &original, &holder, &binding).await.is_ok());
        }
    }
}

/// Reproduce the published pre-validity AAD tuple independently of the new
/// helper, encrypt real material with it, and open through the current adapter.
/// This is byte/cryptographic compatibility for absent metadata, not a fallback
/// accepting an old AAD for a new metadata-bearing publication.
#[tokio::test]
async fn absent_source_validity_opens_the_original_authenticated_envelope_format() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keys.json");
    write(&path, &document());
    let adapter = RecipientKeyEnvelope::open(&path, awaken_time::system_clock()).unwrap();
    let keys = RecipientKeyEnvelope::read_document(&path).unwrap();
    for boundary in [PlaintextBoundary::Worker, PlaintextBoundary::Workload] {
        let req = request(boundary);
        let key = keys
            .keys
            .iter()
            .find(|key| key.holder == req.selected_holder)
            .unwrap();
        let expires = adapter.clock.now_ms() + 3_600_000;
        let original_aad = serde_json::to_vec(&(
            PREFIX,
            &req.access.credential,
            req.access.material_source,
            &req.access.target,
            &req.access.usage,
            &req.access.policy,
            &req.selected_holder,
            &req.binding,
            &key.id,
            expires,
        ))
        .unwrap();
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = key
            .cipher()
            .unwrap()
            .encrypt(
                &nonce,
                Payload {
                    msg: req.material.expose_secret().as_bytes(),
                    aad: &original_aad,
                },
            )
            .unwrap();
        let envelope_ref = SealedCredentialEnvelopeRef {
            id: format!(
                "{PREFIX}{}",
                STANDARD.encode(
                    serde_json::to_vec(&SealedPayload {
                        key_id: key.id.clone(),
                        nonce: nonce.to_vec(),
                        ciphertext,
                    })
                    .unwrap()
                )
            ),
            payload_fingerprint: credential_envelope_payload_fingerprint(
                &req.access,
                &req.selected_holder,
                &req.binding,
            ),
        };
        let recipient = req.selected_holder.trust_domain.clone();
        let envelope = match boundary {
            PlaintextBoundary::Worker => CredentialEnvelope::SealedForWorker {
                envelope_ref,
                recipient,
                expires_at_unix_ms: expires,
            },
            PlaintextBoundary::Workload => CredentialEnvelope::SealedForWorkload {
                envelope_ref,
                recipient,
                expires_at_unix_ms: expires,
            },
            PlaintextBoundary::Platform => unreachable!(),
        };
        let access = req.access.with_envelope(envelope);
        assert_eq!(
            opened(&adapter, &access, &req.selected_holder, &req.binding)
                .await
                .unwrap()
                .material
                .into_secret()
                .unwrap()
                .expose_secret(),
            req.material.expose_secret(),
        );
    }
}
