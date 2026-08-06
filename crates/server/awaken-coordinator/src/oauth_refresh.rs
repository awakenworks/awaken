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

    fn bearer_reloader(
        &self,
        credential_id: CredentialSourceId,
        credential_revision: u64,
    ) -> Arc<dyn CredentialRefresher> {
        Arc::new(VaultBearerReloader {
            credential_id,
            credential_revision,
            credentials: self.credentials.clone(),
            secrets: self.secrets.clone(),
        })
    }
}

struct VaultBearerReloader {
    credential_id: CredentialSourceId,
    credential_revision: u64,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
}

#[async_trait::async_trait]
impl CredentialRefresher for VaultBearerReloader {
    async fn refresh(&self, _challenge: &AuthChallenge) -> Option<Credential> {
        let source = self.credentials.get(&self.credential_id).await.ok()?;
        if source.status != CredentialStatus::Active
            || u64::try_from(source.version).ok()? != self.credential_revision
        {
            return None;
        }
        let bearer = self.secrets.get(source.material_ref.as_ref()?).await.ok()?;
        Some(Credential::Bearer(bearer.expose_secret().to_owned()))
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

    #[tokio::test]
    async fn pinned_bearer_reload_decision_table() {
        // Bearer-reload FMECA / cause-effect graph: C1 the frozen credential id
        // exists; C2 lifecycle is Active; C3 revision equals the Session pin;
        // C4 its exact material reference resolves. Effect E1 returns only the
        // current bytes behind that pin; every other combination yields E2 None
        // before retry. F1 identity widening (S10,O3,D2,RPN60), F2 revoked-use
        // (S10,O3,D2,RPN60), and F3 missing material (S8,O4,D2,RPN64) fail closed.
        //
        // | Rule | exists | active | revision | material | Effect |
        // |---|---|---|---|---|---|
        // | B1 | yes | yes | exact | present | E1 |
        // | B2 | yes | no | exact | present | E2/F2 |
        // | B3 | yes | yes | changed | present | E2/F1 |
        // | B4 | yes | yes | exact | missing | E2/F3 |
        // | B5 | no | any | any | any | E2 |
        let secret_ref = SecretRef("secret:run-mcp".into());
        let secrets: Arc<dyn SecretStore> =
            Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        secrets
            .put(&secret_ref, RedactedString::new("current-bearer"))
            .await
            .unwrap();
        let credentials: Arc<dyn CredentialRepo> =
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let source = awaken_credential_vault::CredentialSource {
            id: CredentialSourceId("cred:run-mcp".into()),
            workspace_id: "workspace".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("awaken-flow/run-mcp".into()),
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: Some(secret_ref.clone()),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        };
        credentials.put(source.clone()).await.unwrap();
        let factory = VaultRefreshFactory::new(credentials.clone(), secrets.clone());
        let challenge = AuthChallenge {
            status: 401,
            www_authenticate: None,
        };
        assert_eq!(
            awaken_runtime_host::CredentialRefreshFactory::bearer_reloader(
                &factory,
                source.id.clone(),
                1,
            )
            .refresh(&challenge)
            .await,
            Some(Credential::Bearer("current-bearer".into())),
            "B1/E1"
        );

        for (case, replacement) in [
            (
                "B2/E2/F2",
                awaken_credential_vault::CredentialSource {
                    status: CredentialStatus::Disabled,
                    ..source.clone()
                },
            ),
            (
                "B3/E2/F1",
                awaken_credential_vault::CredentialSource {
                    version: 2,
                    ..source.clone()
                },
            ),
            (
                "B4/E2/F3",
                awaken_credential_vault::CredentialSource {
                    material_ref: Some(SecretRef("secret:missing".into())),
                    ..source.clone()
                },
            ),
        ] {
            let case_credentials: Arc<dyn CredentialRepo> =
                Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
            case_credentials.put(replacement).await.unwrap();
            let case_factory = VaultRefreshFactory::new(case_credentials, secrets.clone());
            assert_eq!(
                awaken_runtime_host::CredentialRefreshFactory::bearer_reloader(
                    &case_factory,
                    source.id.clone(),
                    1,
                )
                .refresh(&challenge)
                .await,
                None,
                "{case}"
            );
        }
        assert_eq!(
            awaken_runtime_host::CredentialRefreshFactory::bearer_reloader(
                &factory,
                CredentialSourceId("cred:missing".into()),
                1,
            )
            .refresh(&challenge)
            .await,
            None,
            "B5/E2"
        );
    }
}
