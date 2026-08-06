//! Coordinator-owned OAuth refresh and exact Vault rotation adapter.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::repo::{
    CredentialMaterialPatch, CredentialRepo, rotate_credential_materials_exact,
};
use awaken_credential_vault::{
    CredentialStatus, OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretRef, SecretStore,
};
use awaken_ext_mcp::{AuthChallenge, Credential, CredentialRefresher};
use awaken_runtime_contract::{CredentialRefreshAccess, TokenEndpointAuth};
use base64::Engine as _;

pub struct VaultRefreshFactory {
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
}

impl VaultRefreshFactory {
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            credentials,
            secrets,
        }
    }
}

impl awaken_runtime_host::CredentialRefreshFactory for VaultRefreshFactory {
    fn refresher(
        &self,
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
    ) -> Arc<dyn CredentialRefresher> {
        Arc::new(VaultRefresher::new(
            credential_id,
            access,
            self.credentials.clone(),
            self.secrets.clone(),
        ))
    }
}

pub struct VaultRefresher {
    credential_id: CredentialSourceId,
    access: tokio::sync::Mutex<CredentialRefreshAccess>,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    http: reqwest::Client,
}

impl VaultRefresher {
    #[must_use]
    pub fn new(
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        let http = http_client_for(&access.token_endpoint);
        Self {
            credential_id,
            access: tokio::sync::Mutex::new(access),
            credentials,
            secrets,
            http,
        }
    }
}

fn basic_client_auth(client_id: &str, client_secret: &str) -> String {
    let encode =
        |value: &str| form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>();
    let pair = format!("{}:{}", encode(client_id), encode(client_secret));
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

fn http_client_for(url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
    {
        builder = builder.no_proxy();
    }
    builder.build().expect("build OAuth HTTP client")
}

#[async_trait::async_trait]
impl CredentialRefresher for VaultRefresher {
    async fn refresh(&self, _challenge: &AuthChallenge) -> Option<Credential> {
        let mut access = self.access.lock().await;
        if !access.has_valid_client_authentication_binding()
            || !access.has_valid_configuration_fingerprint()
        {
            return None;
        }
        let source = self.credentials.get(&self.credential_id).await.ok()?;
        if source.status != CredentialStatus::Active
            || u64::try_from(source.version).ok()? != access.credential_revision
            || source.material_ref.as_ref().map(|reference| &reference.0)
                != Some(&access.access_token_ref)
            || source
                .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
                .map(|reference| &reference.0)
                != Some(&access.refresh_token_ref)
            || access.client_secret_ref.as_ref()
                != source
                    .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                    .map(|reference| &reference.0)
        {
            return None;
        }
        let refresh_token = self
            .secrets
            .get(&SecretRef(access.refresh_token_ref.clone()))
            .await
            .ok()?;
        let client_secret = match access.token_endpoint_auth {
            TokenEndpointAuth::ClientSecretBasic | TokenEndpointAuth::ClientSecretPost => Some(
                self.secrets
                    .get(&SecretRef(access.client_secret_ref.as_ref()?.clone()))
                    .await
                    .ok()?,
            ),
            TokenEndpointAuth::None => None,
        };
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose_secret()),
        ];
        let mut request = self.http.post(&access.token_endpoint);
        match access.token_endpoint_auth {
            TokenEndpointAuth::None => form.push(("client_id", access.client_id.as_str())),
            TokenEndpointAuth::ClientSecretBasic => {
                request = request.header(
                    reqwest::header::AUTHORIZATION,
                    basic_client_auth(&access.client_id, client_secret.as_ref()?.expose_secret()),
                );
            }
            TokenEndpointAuth::ClientSecretPost => {
                form.push(("client_id", access.client_id.as_str()));
                form.push(("client_secret", client_secret.as_ref()?.expose_secret()));
            }
        }
        if let Some(scope) = &access.scope {
            form.push(("scope", scope.as_str()));
        }
        if let Some(resource) = &access.resource {
            form.push(("resource", resource.as_str()));
        }
        let response = request.form(&form).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        let access_token = body.get("access_token")?.as_str()?.to_owned();
        let mut auxiliary = BTreeMap::new();
        if let Some(rotated) = body.get("refresh_token").and_then(|value| value.as_str()) {
            auxiliary.insert(
                OAUTH_REFRESH_TOKEN_SLOT.to_owned(),
                Some(RedactedString::new(rotated.to_owned())),
            );
        }
        let rotated = rotate_credential_materials_exact(
            &self.credential_id,
            i64::try_from(access.credential_revision).ok()?,
            CredentialMaterialPatch {
                primary: Some(RedactedString::new(access_token.clone())),
                auxiliary,
            },
            self.secrets.as_ref(),
            self.credentials.as_ref(),
        )
        .await
        .ok()?;
        *access = CredentialRefreshAccess::new(
            u64::try_from(rotated.version).ok()?,
            access.token_endpoint.clone(),
            access.client_id.clone(),
            access.token_endpoint_auth,
            rotated
                .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                .map(|reference| reference.0.clone()),
            rotated
                .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)?
                .0
                .clone(),
            rotated.material_ref.as_ref()?.0.clone(),
            access.scope.clone(),
            access.resource.clone(),
        );
        Some(Credential::Bearer(access_token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tampered_exact_refresh_configuration_fails_before_secret_or_network_access() {
        // Causes: C1 exact fingerprint remains valid; C2 an executable OAuth
        // fact changes. Effects: E1 exact revision may refresh; E2 tampering
        // fails before Vault/network access. R1 C1&&!C2 -> E1 is covered by the
        // MCP integration suite; R2 C2 -> E2 is isolated here.
        let mut access = CredentialRefreshAccess::new(
            1,
            "https://auth.example/token".into(),
            "client".into(),
            TokenEndpointAuth::None,
            None,
            "refresh-ref".into(),
            "access-ref".into(),
            None,
            None,
        );
        access.scope = Some("tampered".into());
        let refresher = VaultRefresher::new(
            CredentialSourceId("cred:test".into()),
            access,
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        );
        assert_eq!(
            refresher
                .refresh(&AuthChallenge {
                    status: 401,
                    www_authenticate: None,
                })
                .await,
            None,
            "R2/E2"
        );
    }
}
