//! Policy-bound publication reconciliation.
//!
//! This remains an operation on [`ConfigService`]; the module split isolates
//! dependency-refresh behavior without creating another service or authority.

use awaken_agent_config::{ConfigRegistry, ConfigWrite};
use awaken_executable_agent_contract::ExecutableAgentRegistrationError;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use crate::{ConfigService, PublishError};

pub(crate) async fn reconcile(
    service: &ConfigService,
    workspace: &ScopeId,
    registry: &dyn ConfigRegistry,
    id: &str,
    catalog: &[ToolDescriptor],
) -> Result<bool, String> {
    let stored = registry
        .get_config_revision(id)
        .await
        .map_err(|e| e.to_string())?;
    match stored {
        // Policy selections refresh their authority-owned pins; an operator's
        // concrete pinned binding remains authoritative.
        Some(versioned) if versioned.config.model_binding.requires_reconciliation() => {
            let preview = service
                .preview_publication(workspace, registry, id, catalog)
                .await
                .map_err(|error| error.to_string())?;
            if registry
                .get_publication(&preview.fingerprint)
                .await
                .map_err(|error| error.to_string())?
                .is_some()
            {
                // Exact policy fact replay: preserve the authored revision
                // and reuse the ordinary idempotent registration path. A
                // legacy store may already contain two fingerprints at this
                // revision; only that semantic registrar conflict falls
                // through to the CAS migration below.
                match service.publish(workspace, registry, id, catalog).await {
                    Ok(_) => return Ok(true),
                    Err(PublishError::Registration(
                        ExecutableAgentRegistrationError::Conflict(_),
                    )) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }

            // A changed dependency produces a different executable fact.
            // `(Workspace, Agent, source_revision)` is immutable, so advance
            // the same authoring intent with CAS before publication instead
            // of persisting a conflicting fingerprint at the old revision.
            let next_revision = match registry
                .put_config_if_revision(&versioned.config, versioned.revision)
                .await
                .map_err(|error| error.to_string())?
            {
                ConfigWrite::Applied { revision } => revision,
                ConfigWrite::Conflict { current_revision } => {
                    return Err(PublishError::StaleRevision(current_revision).to_string());
                }
            };
            service
                .publish_at_revisions(workspace, registry, id, catalog, Some(next_revision), None)
                .await
                .map_err(|error| error.to_string())?;
            Ok(true)
        }
        _ => Ok(false),
    }
}
