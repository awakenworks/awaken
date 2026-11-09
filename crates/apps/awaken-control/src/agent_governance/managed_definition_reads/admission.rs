// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Verified direct runtime readers still need exact native definition
// @formal reason: and independent runtime decisions within one current IAM fence.
//! Canonical full-DTO read admission; never an owner or definition-write identity.
use super::*;
use crate::agent_governance::routes::ordinary;
use awaken_iam_contract::{PrincipalRef, ProductId};
pub(in crate::agent_governance) struct DefinitionAdmission {
    iam: Arc<RemoteManagementAuthz>,
    pub(in crate::agent_governance) principal: PrincipalRef,
    issuer: String,
    token: awaken_agent_contract::RedactedString,
    workspace: String,
    fence: String,
    product: ProductId,
}
async fn subject(
    iam: Arc<RemoteManagementAuthz>,
    token: awaken_agent_contract::RedactedString,
) -> Result<(String, PrincipalRef), Failure> {
    crate::authz::off_event_loop::run(move || {
        iam.current_agent_definition_identity(token.expose_secret())
    })
    .await
    .map_err(|_| Failure::Unavailable)?
    .map_err(|error| match error {
        awaken_iam_host::AuthReject::Unavailable => Failure::Unavailable,
        _ => Failure::Unauthorized,
    })
}
impl DefinitionAdmission {
    pub(in crate::agent_governance) async fn begin(
        iam: Arc<RemoteManagementAuthz>,
        config: Option<&AgentGovernanceConfig>,
        context: &RequestContext,
    ) -> Result<Self, Failure> {
        let product = config.ok_or(Failure::NotConfigured)?.product_id.clone();
        let (issuer, principal) = subject(iam.clone(), context.token.clone()).await?;
        ordinary(context, &context.workspace).await?;
        let (current, who, workspace) = (iam.clone(), principal.clone(), context.workspace.clone());
        let fence = crate::authz::off_event_loop::run(move || {
            authority::observe_principal(&current, &who, &workspace)
        })
        .await
        .map_err(|_| Failure::Unavailable)??;
        Ok(Self {
            iam,
            principal,
            issuer,
            product,
            fence,
            token: context.token.clone(),
            workspace: context.workspace.clone(),
        })
    }
    pub(in crate::agent_governance) fn fence(&self) -> &str {
        &self.fence
    }
    pub(in crate::agent_governance) async fn allows(
        &self,
        id: &str,
        demand: &'static str,
    ) -> Result<bool, Failure> {
        // This narrow reader cannot be reused for preferences or definition writes.
        if !matches!(demand, authority::DEFINITION_READ | authority::USE) {
            return Err(Failure::Denied);
        }
        let (iam, product, issuer, principal, workspace, id, fence) = (
            self.iam.clone(),
            self.product.clone(),
            self.issuer.clone(),
            self.principal.clone(),
            self.workspace.clone(),
            id.to_owned(),
            self.fence.clone(),
        );
        crate::authz::off_event_loop::run(move || {
            authority::allows_principal(
                &iam,
                &product,
                authority::DefinitionSubject {
                    issuer: &issuer,
                    principal: &principal,
                },
                &workspace,
                &id,
                demand,
                &fence,
            )
        })
        .await
        .map_err(|_| Failure::Unavailable)?
    }
    pub(in crate::agent_governance) async fn end(
        &self,
        context: &RequestContext,
    ) -> Result<(), Failure> {
        ordinary(context, &self.workspace).await?;
        let (issuer, principal) = subject(self.iam.clone(), self.token.clone()).await?;
        if issuer != self.issuer || principal != self.principal {
            return Err(Failure::Unauthorized);
        }
        let (iam, workspace) = (self.iam.clone(), self.workspace.clone());
        let fence = crate::authz::off_event_loop::run(move || {
            authority::observe_principal(&iam, &principal, &workspace)
        })
        .await
        .map_err(|_| Failure::Unavailable)??;
        if fence != self.fence {
            return Err(Failure::Unavailable);
        }
        Ok(())
    }
}
