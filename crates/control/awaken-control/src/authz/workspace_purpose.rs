// @formal surface exclude=composition_adapter
// @formal reason: Consumes a fresh purpose observation from the configured hosted owner.
// @formal reason: The adapter owns no purpose row, policy, identity or authorization decision.
//! Configured transport for the hosted ordinary Workspace observation.

use std::{path::PathBuf, sync::Arc, time::Duration};

use awaken_outbound_http::{HttpReply, InternalHttpClient, JsonRequest, normalize_http_base_url};
use awaken_service_auth_contract::{
    ProjectedFileServiceBearerTokenSource, ServiceBearerTokenSource, resolve_service_bearer_token,
};
use awaken_tenancy::workspace_purpose::{OrdinaryWorkspaceAdmission, WorkspacePurposeUnavailable};

/// One current read per admitted request. Endpoint and credential custody are
/// selected by the hosted composition before any request is served.
pub struct HttpOrdinaryWorkspaceAdmission {
    endpoint: String,
    audience: String,
    client: InternalHttpClient,
    tokens: Arc<dyn ServiceBearerTokenSource>,
}

impl HttpOrdinaryWorkspaceAdmission {
    pub(super) fn sibling_endpoint(&self, endpoint: &str) -> Result<Self, String> {
        let endpoint = normalize_http_base_url(endpoint).map_err(|error| error.to_string())?;
        Ok(Self {
            endpoint,
            audience: self.audience.clone(),
            client: self.client.clone(),
            tokens: self.tokens.clone(),
        })
    }

    pub(super) async fn observe_json(
        &self,
        body: &str,
    ) -> Result<HttpReply, WorkspacePurposeUnavailable> {
        let token = resolve_service_bearer_token(self.tokens.as_ref())
            .map_err(|_| WorkspacePurposeUnavailable)?;
        let request = JsonRequest::post(&self.endpoint)
            .bearer(token.as_ref())
            .header("x-iam-audience", &self.audience)
            .json_body(body);
        self.client
            .send(&request)
            .await
            .map_err(|_| WorkspacePurposeUnavailable)
    }

    pub fn new(
        endpoint: &str,
        audience: &str,
        tokens: Arc<dyn ServiceBearerTokenSource>,
    ) -> Result<Self, String> {
        let endpoint = normalize_http_base_url(endpoint).map_err(|error| error.to_string())?;
        if audience.is_empty()
            || audience.trim() != audience
            || audience.chars().any(char::is_control)
        {
            return Err("invalid Workspace purpose audience".into());
        }
        resolve_service_bearer_token(tokens.as_ref())?;
        Ok(Self {
            endpoint,
            audience: audience.to_owned(),
            client: InternalHttpClient::new(Duration::from_secs(3))
                .map_err(|error| error.to_string())?,
            tokens,
        })
    }

    pub fn from_projected_service_token(
        endpoint: &str,
        audience: &str,
        path: PathBuf,
    ) -> Result<Self, String> {
        Self::new(
            endpoint,
            audience,
            Arc::new(ProjectedFileServiceBearerTokenSource::new(
                path,
                "Workspace purpose",
            )),
        )
    }
}

#[async_trait::async_trait]
impl OrdinaryWorkspaceAdmission for HttpOrdinaryWorkspaceAdmission {
    async fn is_ordinary_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<bool, WorkspacePurposeUnavailable> {
        let body = serde_json::json!({ "workspace_id": workspace_id }).to_string();
        let reply = self.observe_json(&body).await?;
        match (reply.status(), reply.body().is_empty()) {
            (204, true) => Ok(true),
            (404, true) => Ok(false),
            _ => Err(WorkspacePurposeUnavailable),
        }
    }
}
