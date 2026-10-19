//! Original registration/retirement fault fixture; no production state lives here.
use super::resource_registry::FaultInjectingResourceRegistry;
use awaken_credential_contract::{
    CredentialMaterialIngress, CredentialMaterialIngressCommand,
    CredentialMaterialRetirementCommand, CredentialMaterialRotationCommand,
};

use awaken_credential_contract::MaterialEntry;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Fault-only ingress port. Its set exists only to project created/replayed and
/// retirement observations for the cause/effect rules; production Vault state
/// and replay remain owned by the composed credential implementation.
#[derive(Default)]
pub(super) struct FaultInjectingCredentialMaterialIngress {
    pub(super) calls: AtomicUsize,
    failures: AtomicUsize,
    retirement_failures: AtomicUsize,
    entered_sources: Mutex<BTreeSet<String>>,
    retirements: Mutex<Vec<awaken_credential_contract::CredentialRef>>,
}

impl FaultInjectingCredentialMaterialIngress {
    pub(super) fn fail_next(&self) {
        self.failures.store(1, Ordering::SeqCst);
    }

    pub(super) fn fail_next_retirement(&self) {
        self.retirement_failures.store(1, Ordering::SeqCst);
    }

    pub(super) fn seed_replayed_source(&self, repository_id: &str) {
        self.entered_sources
            .lock()
            .unwrap()
            .insert(format!("{repository_id}:credential"));
    }

    pub(super) fn retirements(&self) -> Vec<awaken_credential_contract::CredentialRef> {
        self.retirements.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CredentialMaterialIngress for FaultInjectingCredentialMaterialIngress {
    async fn enter_material(
        &self,
        command: CredentialMaterialIngressCommand,
    ) -> Result<MaterialEntry, String> {
        let source_id = command.source_id;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if FaultInjectingResourceRegistry::consume_failure(&self.failures) {
            Err("injected credential ingress failure".into())
        } else {
            let created = self
                .entered_sources
                .lock()
                .unwrap()
                .insert(source_id.0.clone());
            Ok(MaterialEntry {
                credential: awaken_credential_contract::CredentialRef {
                    id: source_id.0,
                    revision: 1,
                },
                created,
            })
        }
    }

    async fn rotate_material(
        &self,
        _command: CredentialMaterialRotationCommand,
    ) -> Result<u64, String> {
        Err("rotation is outside this registration test".into())
    }

    async fn retire_material(
        &self,
        command: CredentialMaterialRetirementCommand,
    ) -> Result<(), String> {
        if FaultInjectingResourceRegistry::consume_failure(&self.retirement_failures) {
            return Err("injected credential retirement failure".into());
        }
        self.retirements.lock().unwrap().push(command.credential);
        Ok(())
    }
}
