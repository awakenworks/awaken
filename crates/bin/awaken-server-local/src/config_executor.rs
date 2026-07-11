//! The config-plane-backed model executor provider: resolves a session's
//! `model_ref` to a real executor from the authored catalog + the workspace's
//! credential, so the console configures models through the API and the runtime
//! makes real calls — never an `AWAKEN_MODEL_SOURCE` env shortcut.
//!
//! Resolution chain (our single-tenant scenario, collapsed):
//!   `model_ref → offering(provider) → the workspace's first Active credential that
//!    `can_consume` that provider → resolve_inference → executor_from_resolved`.
//!
//! `ExecutorProvider::executor_for` is sync but the stores are async; it bridges
//! via `block_in_place` + the ambient runtime handle (the same pattern the
//! management stores use). Returning `None` falls back to the host's default
//! executor, so an unconfigured/unresolvable model still runs on the built-in
//! scenario model rather than erroring.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_config_resolver::{can_consume, resolve_inference};
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{CredentialBinding, CredentialSource, CredentialStatus, SecretStore};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::ExecutorProvider;

use crate::executor_from_resolved;

/// Resolves `model_ref → executor` from the live config plane.
pub struct ConfigExecutorProvider {
    catalog: Arc<dyn CatalogRepo>,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    /// Single-tenant default (Option A): the workspace whose credentials back runs.
    workspace_id: String,
}

impl ConfigExecutorProvider {
    pub fn new(
        catalog: Arc<dyn CatalogRepo>,
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
        workspace_id: impl Into<String>,
    ) -> Self {
        Self {
            catalog,
            credentials,
            secrets,
            workspace_id: workspace_id.into(),
        }
    }

    /// Resolve an executor from configured state, or `None` to fall back to the
    /// host default (no offering, no compatible credential, or a resolve error).
    async fn resolve(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        let catalog = self.catalog.snapshot().await.ok()?;
        // The offering names the provider whose credential must authenticate the model.
        let provider_id = catalog
            .offerings
            .iter()
            .find(|o| o.model_id == model_ref)?
            .provider_id
            .0
            .clone();
        // Per-provider default derive: the workspace's first Active credential that
        // `can_consume` this provider (the "one default per provider" rule).
        let sources = self.credentials.list(&self.workspace_id).await.ok()?;
        let chosen = sources
            .iter()
            .find(|s| s.status == CredentialStatus::Active && can_consume(&provider_id, s))?;
        let binding = CredentialBinding::Exact {
            credential_source_id: chosen.id.clone(),
        };
        let lookup: HashMap<String, CredentialSource> =
            sources.iter().map(|s| (s.id.0.clone(), s.clone())).collect();
        let inference =
            resolve_inference(&catalog, model_ref, &binding, &lookup, self.secrets.as_ref())
                .await
                .ok()?;
        executor_from_resolved(&inference).ok()
    }
}

impl ExecutorProvider for ConfigExecutorProvider {
    fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        // Bridge the sync port to the async stores on the ambient multi-thread
        // runtime (mirrors the management stores' block_in_place bridge).
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.resolve(model_ref))
        })
    }
}
