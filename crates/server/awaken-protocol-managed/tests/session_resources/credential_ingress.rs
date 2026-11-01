//! The original wire Credential ingress recorder; no case or assertion moves here.

#[derive(Default)]
pub(super) struct CountingCredentialMaterialIngress {
    pub(super) writes: std::sync::atomic::AtomicUsize,
    pub(super) retirements: std::sync::atomic::AtomicUsize,
    pub(super) sources: std::sync::Mutex<Vec<String>>,
    pub(super) memory_change_after_first_write: std::sync::Mutex<
        Option<(
            std::sync::Arc<dyn awaken_resource_contract::ResourceAdministration>,
            awaken_resource_contract::ChangeMemoryStoreState,
        )>,
    >,
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialMaterialIngress for CountingCredentialMaterialIngress {
    async fn enter_material(
        &self,
        command: awaken_credential_contract::CredentialMaterialIngressCommand,
    ) -> Result<awaken_credential_contract::MaterialEntry, String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.sources
            .lock()
            .unwrap()
            .push(command.source_id.0.clone());
        // A test may change real Registry state at this observed effect
        // boundary; the Registry and its normal resolver still own the result.
        let pending = self.memory_change_after_first_write.lock().unwrap().take();
        if let Some((registry, change)) = pending {
            registry
                .change_memory_store_state(change)
                .await
                .map_err(|error| format!("Memory fixture state change failed: {error}"))?;
        }
        Ok(awaken_credential_contract::MaterialEntry {
            credential: awaken_agent_contract::credential::CredentialRef {
                id: command.source_id.0,
                revision: 1,
            },
            created: true,
        })
    }

    async fn rotate_material(
        &self,
        command: awaken_credential_contract::CredentialMaterialRotationCommand,
    ) -> Result<u64, String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        command
            .expected_revision
            .checked_add(1)
            .ok_or_else(|| "repository credential revision overflow".into())
    }

    async fn retire_material(
        &self,
        _command: awaken_credential_contract::CredentialMaterialRetirementCommand,
    ) -> Result<(), String> {
        self.retirements
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
