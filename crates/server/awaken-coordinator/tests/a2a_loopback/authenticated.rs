//! One authenticated TCP peer for execution and durable cancellation composition.
use super::*;
use std::sync::Mutex;

// One observation buffer for the peer and middleware; existing authenticated
// execution/cancellation tests assert method, path and authorization together.
type PeerRequests = Arc<Mutex<Vec<(String, String, bool)>>>;

pub(super) struct AuthenticatedPeer {
    pub endpoint: String,
    pub access: CredentialAccess,
    pub security_fingerprint: String,
    pub requests: PeerRequests,
    credentials: Arc<InMemoryCredentialRepo>,
    secrets: Arc<InMemorySecretStore>,
    task: tokio::task::JoinHandle<()>,
}

async fn require_remote_bearer(
    axum::extract::State(requests): axum::extract::State<PeerRequests>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some("Bearer remote-secret");
    requests.lock().unwrap().push((
        request.method().to_string(),
        request.uri().path().to_string(),
        authorized,
    ));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

impl AuthenticatedPeer {
    pub async fn start(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind authenticated A2A peer");
        let endpoint = format!("http://{}", listener.local_addr().expect("peer address"));
        let mut card = awaken_protocol_a2a::agent_card("remote");
        card.url = format!("{endpoint}/v1/a2a");
        card.security_schemes.insert(
            "bearer".into(),
            serde_json::from_value(serde_json::json!({
                "type": "http",
                "scheme": "Bearer"
            }))
            .expect("valid HTTP Bearer security scheme"),
        );
        card.security = serde_json::from_value(serde_json::json!([{"bearer": []}]))
            .expect("valid security requirement");
        let security_fingerprint =
            awaken_runtime_contract::content_fingerprint(&(&card.security_schemes, &card.security))
                .map(|fingerprint| format!("sha256:{fingerprint}"))
                .expect("fingerprint card security");
        let card_router = Router::new().route(
            awaken_protocol_a2a::client::AGENT_CARD_PATH,
            get({
                let card = card.clone();
                move || {
                    let card = card.clone();
                    async move { axum::Json(card) }
                }
            }),
        );
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let authenticated_peer = app.layer(axum::middleware::from_fn_with_state(
            requests.clone(),
            require_remote_bearer,
        ));
        let peer = card_router.fallback_service(authenticated_peer);
        let peer_task = tokio::spawn(async move {
            axum::serve(listener, peer)
                .await
                .expect("serve authenticated A2A peer");
        });

        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let entered = enter_credential(
            CredentialCreateParams {
                workspace_id: "default".into(),
                kind: CredentialKind::Vault,
                provider_id: Some(endpoint.clone()),
                env_key: None,
                secret: Some(RedactedString::new("remote-secret")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("enter origin credential");
        let access = CredentialAccess::new(
            CredentialRef {
                id: entered.id.0,
                revision: u64::try_from(entered.version).expect("positive credential revision"),
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::self_hosted_provider(),
        )
        .with_target(CredentialTarget::new(
            CredentialPurpose::RemoteAgentAuthorization,
            endpoint.clone(),
        ));

        Self {
            endpoint,
            access,
            security_fingerprint,
            requests,
            credentials,
            secrets,
            task: peer_task,
        }
    }

    pub fn materializer(&self) -> awaken_credential_materializer::PinnedCredentialMaterializer {
        awaken_credential_materializer::PinnedCredentialMaterializer::new(
            self.credentials.clone(),
            self.secrets.clone(),
        )
    }

    pub async fn revoke(&self) {
        use awaken_credential_vault::repo::{CredentialRetirement, revoke_credential_exact};
        revoke_credential_exact(
            &awaken_credential_contract::CredentialSourceId(self.access.credential.id.clone()),
            i64::try_from(self.access.credential.revision).unwrap(),
            CredentialRetirement::Disable,
            self.secrets.as_ref(),
            self.credentials.as_ref(),
        )
        .await
        .expect("revoke exact admitted origin credential");
    }
}

impl Drop for AuthenticatedPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
