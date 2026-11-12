//! Exact-version source validity is checked before material I/O.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::repo::InMemoryCredentialRepo;
use crate::{InMemorySecretStore, SecretStore};

struct CountReads {
    inner: InMemorySecretStore,
    reads: AtomicUsize,
    clock: Arc<awaken_lease_testkit::ManualClock>,
    expire_after_read: AtomicBool,
}

impl CountReads {
    fn new(clock: Arc<awaken_lease_testkit::ManualClock>) -> Self {
        Self {
            inner: InMemorySecretStore::new(),
            reads: AtomicUsize::new(0),
            clock,
            expire_after_read: AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl SecretStore for CountReads {
    async fn put(
        &self,
        reference: &SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(reference, secret).await
    }
    async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let material = self.inner.get(reference).await;
        if self.expire_after_read.swap(false, Ordering::SeqCst) {
            self.clock.set(5_000);
        }
        material
    }
    async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(reference).await
    }
    async fn inventory(&self) -> Result<Vec<SecretRef>, CredentialError> {
        self.inner.inventory().await
    }
}

#[tokio::test]
async fn forged_pin_expiry_and_expired_legacy_pin_are_rejected_before_material_reads() {
    let repo = Arc::new(InMemoryCredentialRepo::new());
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000));
    let secrets = Arc::new(CountReads::new(clock.clone()));
    let app = crate::VaultApplication::new(secrets.clone(), repo.clone(), clock.clone(), None);
    let receipt = app
        .enter_application_mcp_bearer(crate::ApplicationMcpBearerEntry {
            workspace_id: "workspace".into(),
            application_authority_id: "expiry-reader".into(),
            mcp_server_url: "https://flow.example.test/mcp".into(),
            target: crate::McpCredentialTarget {
                audience: "https://flow.example.test/mcp".into(),
                fingerprint: "endpoint".into(),
            },
            idempotency_key: "generation-1".into(),
            credential_generation: 1,
            expires_at_unix_ms: Some(5_000),
            bearer: RedactedString::new("test-material"), // awaken-allow(secret): deterministic fixture material
        })
        .await
        .unwrap();
    let target = awaken_agent_contract::credential::CredentialTarget::new(
        awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
        "https://flow.example.test/mcp",
    );
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker.test");
    let access = CredentialAccess::new(
        awaken_agent_contract::credential::CredentialRef {
            id: receipt.source_id.0,
            revision: receipt.revision,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
        awaken_agent_contract::credential::CredentialExecutionPolicy::exact(
            holder.clone(),
            awaken_agent_contract::credential::ModelExposurePolicy::Forbidden,
        ),
    )
    .with_target(target)
    .with_expiry(5_000)
    .with_issuer_generation(1);
    let binding = CredentialMaterialBinding::for_target("workspace", &"mcp", &access.usage);
    let authority = VaultLocalMaterialAuthority::new(repo, secrets.clone(), clock.clone());
    let reads_before = secrets.reads.load(Ordering::SeqCst);
    for expiry in [4_000, 6_000] {
        let changed = access.clone().with_expiry(expiry);
        assert_eq!(
            authority
                .resolve_exact(CredentialMaterialRequest {
                    access: &changed,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .unwrap_err(),
            CredentialMaterialError::BindingMismatch
        );
        assert_eq!(secrets.reads.load(Ordering::SeqCst), reads_before);
    }
    for generation in [0, 2] {
        let changed = access.clone().with_issuer_generation(generation);
        assert_eq!(
            authority
                .resolve_exact(CredentialMaterialRequest {
                    access: &changed,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .unwrap_err(),
            CredentialMaterialError::BindingMismatch
        );
        assert_eq!(secrets.reads.load(Ordering::SeqCst), reads_before);
    }
    authority
        .resolve_exact(CredentialMaterialRequest {
            access: &access,
            selected_holder: &holder,
            binding: &binding,
            expected_provider: None,
        })
        .await
        .unwrap();
    assert_eq!(secrets.reads.load(Ordering::SeqCst), reads_before + 1);
    secrets.expire_after_read.store(true, Ordering::SeqCst);
    assert_eq!(
        authority
            .resolve_exact(CredentialMaterialRequest {
                access: &access,
                selected_holder: &holder,
                binding: &binding,
                expected_provider: None,
            })
            .await
            .unwrap_err(),
        CredentialMaterialError::Expired,
        "expiry during the original material read releases no material"
    );
    assert_eq!(secrets.reads.load(Ordering::SeqCst), reads_before + 2);
    for pin in [access.clone(), {
        let mut legacy = access;
        legacy.expires_at_unix_ms = None;
        legacy.issuer_generation = None;
        legacy
    }] {
        assert_eq!(
            authority
                .resolve_exact(CredentialMaterialRequest {
                    access: &pin,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .unwrap_err(),
            CredentialMaterialError::Expired
        );
        assert_eq!(secrets.reads.load(Ordering::SeqCst), reads_before + 2);
    }
}
