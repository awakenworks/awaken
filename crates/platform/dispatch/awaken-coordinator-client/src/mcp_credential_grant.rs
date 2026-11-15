// @formal evidence worker.secret_materialization_is_claim_fenced
//! A registered Worker's MCP bearer refresh: the one credential grant
//! endpoint, under the Session MCP generation lease that staged the bearer.
//!
//! The Worker holds no Vault or refresh token. On an authentication challenge
//! it names the generation and its pinned credential; the Coordinator decides
//! the grant from the Session aggregate and performs the exact refresh.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::credential::{
    AuthChallenge, Credential, CredentialAccess, CredentialRef, CredentialRefresher,
};
use awaken_dispatch_contract::{
    WorkerCredentialGrant, WorkerCredentialGrantMaterial, WorkerCredentialGrantTransport,
};
use awaken_session_configuration_contract::mcp::{McpCredentialRefresh, McpGenerationRef};

/// The registered Worker's [`McpCredentialRefresh`] over its grant transport.
pub struct WorkerMcpCredentialRefresh {
    transport: Arc<dyn WorkerCredentialGrantTransport>,
}

impl WorkerMcpCredentialRefresh {
    #[must_use]
    pub fn new(transport: Arc<dyn WorkerCredentialGrantTransport>) -> Self {
        Self { transport }
    }
}

impl McpCredentialRefresh for WorkerMcpCredentialRefresh {
    fn refresher(
        &self,
        generation: &McpGenerationRef,
        credential: &CredentialAccess,
    ) -> Arc<dyn CredentialRefresher> {
        Arc::new(GenerationBearerGrant {
            transport: self.transport.clone(),
            generation: generation.clone(),
            credential: credential.credential.clone(),
        })
    }
}

struct GenerationBearerGrant {
    transport: Arc<dyn WorkerCredentialGrantTransport>,
    generation: McpGenerationRef,
    credential: CredentialRef,
}

#[async_trait]
impl CredentialRefresher for GenerationBearerGrant {
    async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential> {
        let answer = self
            .transport
            .credential_grant(WorkerCredentialGrant::McpGeneration {
                generation: self.generation.clone(),
                credential: self.credential.clone(),
                challenge_status: challenge.status,
                www_authenticate: challenge.www_authenticate.clone(),
            })
            .await;
        match answer {
            Ok(Some(WorkerCredentialGrantMaterial::McpGeneration(Some(bearer)))) => Some(
                Credential::Bearer(bearer.into_redacted().expose_secret().to_owned()),
            ),
            Ok(Some(WorkerCredentialGrantMaterial::IssuerMissing(kind))) => {
                tracing::warn!(
                    code = kind.issuer_missing_code(),
                    "the Coordinator issues no {kind} grants; the MCP challenge surfaces"
                );
                None
            }
            // No bearer, a lost generation lease, or another holding: the
            // original challenge surfaces.
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, "MCP credential grant was not answered");
                None
            }
        }
    }
}
