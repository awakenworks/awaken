//! Google OAuth 2.0 token minting via JWT-Bearer flow (RFC 7523).
//!
//! Given a parsed Google service-account JSON key, this signer:
//! 1. builds a JWT with `{iss=client_email, scope, aud=token_uri,
//!    exp=now+3600s, iat=now}`,
//! 2. signs it with RS256 using the key's private RSA key,
//! 3. POSTs to the `token_uri` with `grant_type=urn:ietf:params:oauth:
//!    grant-type:jwt-bearer&assertion={jwt}`,
//! 4. parses the response into a [`Token`] with the access_token and
//!    expiry derived from the `expires_in` field.
//!
//! Caching, single-flight, and rotation are the broker's responsibility;
//! this module is a pure mint primitive.
//!
//! ## Why we sign in-process (no `gcp_auth` crate)
//! `gcp_auth` and similar crates default to **ambient credential
//! discovery**: env vars, `~/.config/gcloud/`, GCE metadata server. That
//! conflicts with awaken's design tenet that all provider configuration
//! lives in `ProviderSpec` and only there. Implementing JWT signing
//! ourselves (~80 lines) keeps the credential surface auditable.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use super::error::CredentialError;
use super::material::GoogleServiceAccountKey;
use super::token::Token;

/// JWT lifetime. Google accepts up to 3600s; using the maximum minimises
/// signing churn for high-throughput agents (the broker still refreshes
/// the access_token before its actual expiry, not the JWT's).
const JWT_LIFETIME_SECS: u64 = 3600;

#[derive(Serialize)]
struct JwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    exp: u64,
    iat: u64,
}

#[derive(Deserialize)]
struct OAuthResponse {
    access_token: String,
    /// Lifetime in seconds. Google currently emits 3599 or 3600.
    expires_in: u64,
}

/// Mint a Google OAuth access token. See module docs.
///
/// `scope` is forwarded verbatim to Google. The caller (broker) defaults
/// to `https://www.googleapis.com/auth/cloud-platform` when the
/// `ProviderSpec.adapter_options.scopes` field is absent.
pub(super) async fn mint(
    provider_id: &str,
    key: &Arc<GoogleServiceAccountKey>,
    scope: &str,
    http: &reqwest::Client,
) -> Result<Token, CredentialError> {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| CredentialError::SigningFailed {
            provider_id: provider_id.to_owned(),
            reason: format!("system clock before UNIX epoch: {e}"),
        })?
        .as_secs();

    // 1. Build & sign the JWT.
    let claims = JwtClaims {
        iss: &key.client_email,
        scope,
        aud: &key.token_uri,
        exp: now_secs + JWT_LIFETIME_SECS,
        iat: now_secs,
    };
    let encoding_key = EncodingKey::from_rsa_pem(key.private_key.expose_secret().as_bytes())
        .map_err(|e| CredentialError::SigningFailed {
            provider_id: provider_id.to_owned(),
            reason: format!("private key not a valid RSA PEM: {e}"),
        })?;
    let assertion = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding_key)
        .map_err(|e| CredentialError::SigningFailed {
            provider_id: provider_id.to_owned(),
            reason: format!("RS256 sign failed: {e}"),
        })?;

    // 2. Exchange JWT → OAuth token.
    let response = http
        .post(&key.token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &assertion),
        ])
        .send()
        .await
        .map_err(|e| classify_reqwest_error(provider_id, e))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(if status.is_server_error() {
            CredentialError::TransientUpstream {
                provider_id: provider_id.to_owned(),
                reason: format!("status {status}: {body}"),
            }
        } else {
            CredentialError::PermanentUpstream {
                provider_id: provider_id.to_owned(),
                status: status.as_u16(),
                body,
            }
        });
    }

    let oauth: OAuthResponse =
        response
            .json()
            .await
            .map_err(|e| CredentialError::TransientUpstream {
                provider_id: provider_id.to_owned(),
                reason: format!("token response not valid JSON: {e}"),
            })?;

    Ok(Token {
        bearer: awaken_contract::secret::RedactedString::new(oauth.access_token),
        expires_at: SystemTime::now() + Duration::from_secs(oauth.expires_in),
    })
}

/// Discriminate retryable transport faults (DNS, connection reset, TLS
/// handshake) from upstream-rejected requests. reqwest doesn't give us
/// a clean enum so the only signal we have is whether a status code
/// reached us.
fn classify_reqwest_error(provider_id: &str, e: reqwest::Error) -> CredentialError {
    if e.is_timeout() || e.is_connect() || e.is_request() || e.status().is_none() {
        CredentialError::Network {
            provider_id: provider_id.to_owned(),
            reason: e.to_string(),
        }
    } else if let Some(status) = e.status() {
        if status.is_server_error() {
            CredentialError::TransientUpstream {
                provider_id: provider_id.to_owned(),
                reason: e.to_string(),
            }
        } else {
            CredentialError::PermanentUpstream {
                provider_id: provider_id.to_owned(),
                status: status.as_u16(),
                body: String::new(),
            }
        }
    } else {
        CredentialError::Network {
            provider_id: provider_id.to_owned(),
            reason: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::material::GoogleServiceAccountKey;
    use awaken_contract::secret::RedactedString;
    use std::sync::Arc;

    /// Generated locally with `openssl genpkey -algorithm RSA -pkeyopt
    /// rsa_keygen_bits:2048`. Embedded so JWT signing can be exercised
    /// without filesystem I/O. **Not a real Google key** — only used to
    /// verify the signing path locally; the OAuth exchange is mocked
    /// separately.
    const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCqSsIrl4nIivKr
bS9Fj6p2QVG7eYNzwvprEDUoWXkeHRQyworv676NUHy7VkB4/deLuhHT48Z3nNGd
ViBuFMHIUqfuEG7rjiR2B3Ln2RtwGnhbPSuSLpMjBD65q3AztSPc5OvZYyElQE87
UGA7af4CO59MqQAMYEMwxKYt+Il0Ko+ntwG3lY7x/g5DZk11CmkrRYQOt1aj7TwR
rDokJYjc/ixKHw3FvBiWg33ez0covf/5kCqdp8VnHFeQwD+hHLJT0qKPuXyGcqRB
DG0YYOGpJ29k0m/DaxwJrSSCNleaOk2qof9VfLdSPGKv47m0XGUVcYELgWFYN6IK
6ARhFUYHAgMBAAECggEAChlF24zwjFaFHppqf78N1lZ4UNxbac2JyTicVmi78Ie7
72ivEZxS4BGCXB+40hQHqM8fiIfM+MHxglmdsbmEZmtUbx9FXK3AxskZTNuIr4S6
V3rQryoY6q4xRBSBImffGRXwUKN6zzk5maRiGJPoDtzXTRYGnTVGNsmqqzY+fIeC
xvoyIQv+qwJ+gdh+yaaCnzsOXgYSAXrGbCRLQxMPNQL5gEcaxZCp/hewWYRkCphJ
0ROoWpXCSJdum5zks7/WmEoB2IsdyEvOzKxFEDgX1OFYkMxym66g9kAekCNSzHjw
BcgzrImQVGj0cbb1RmBfkUWYZ+LdFrdMryuHQrRb3QKBgQDh8uUDcoZBJ45U/kLp
5iwJGND6dRH8XBRDrF6KNq5u+MI6GnSC4ViBHgTqEAcBXHWyDBFA7mcuaQ9Bk2VD
m9utge6mf4ZOJLocEflpfwz9D4uf1BZXDFe1OShAH/2ji6hadNn8BKeEJFNB1dQB
r1W+pT4BiOrFKtn4A4sTLSd2IwKBgQDA8NyXVVqtRIOVBQwpSY4HsWKuN9Wh+c6i
o8QX2Dv3EzhBrGpItyyWVYGLl+41mFsdp5PUsHak3h+U+3HztizwZGBm/5ILR3+/
HrOFdeE+nKB9tqyoCaCEO6ZVO7qAoGvpN2nKSlQ8svr0nA1ZJWx8xD1El2sKEw79
fKD9TrJkzQKBgBNw+eGVDhY3GBkaE5nakzlpKDoUrqp/JcM45p2P3OxxfQzQz+uf
BiV99sBJBsFIOlxKi4WBveERax2iWBk8JOfGAUnUOTMqF9VoeoRoSS7REpt6/T0a
M8XFGECEQCe9UYwO996ma9+D3KISiv5mHsObpj0tkb3LVRvw+ht5TCbvAoGAAuxj
W0Om0RNFrx9ZdNKxfTpZ1WvxJ7giQmKa2QWkuvSmmJAlOB7WZRy8jsHpkRRS5Rsh
6UoXMh5PejFpI5kyCx5qO4VJ0DPwIpQzgiUsGYfEAsOe0Bj9PqOsvIPgKozDtc/q
IW+I4TaRCN3Icf5YK3fJud1VeNybEIov4kar+00CgYEAj1W2JhamBtQ+S7tucbOZ
innvyZodDLuNi7hwjT561nn3Hdu+MllZXTlXxY9Oynz8I+X5MyRv4tuuIHLQfLFt
cQo8sceDvBplt1vJ0FptIav9/LeMeHMuoRDJXD1Q2nTpcodeFPelq0MXKCBGNt1Q
yJe99wYtUpHld4D+pLiyUnQ=
-----END PRIVATE KEY-----";

    fn test_sa_key(token_uri: &str) -> Arc<GoogleServiceAccountKey> {
        Arc::new(GoogleServiceAccountKey {
            client_email: "test-sa@test-project.iam.gserviceaccount.com".to_owned(),
            private_key: RedactedString::new(TEST_PRIVATE_KEY_PEM.to_owned()),
            token_uri: token_uri.to_owned(),
            project_id: Some("test-project".to_owned()),
        })
    }

    /// Local HTTP server that responds with a configured status + body.
    /// Returned URL is the address of the started listener; caller should
    /// drop the JoinHandle when finished.
    async fn spawn_mock_token_endpoint(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/token");
        let handle = tokio::spawn(async move {
            // One-shot: accept first connection and respond.
            let (mut sock, _) = listener.accept().await.expect("accept");
            // Drain request headers + body.
            // Best-effort read — the test only cares that we sent a response.
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                let mut total = 0;
                let mut headers_done = false;
                let mut content_length: usize = 0;
                let mut header_buf = Vec::new();
                let mut reader = BufReader::new(&mut sock);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    if line == "\r\n" {
                        headers_done = true;
                        break;
                    }
                    header_buf.extend_from_slice(line.as_bytes());
                    if let Some(rest) = line.strip_prefix("Content-Length: ") {
                        content_length = rest.trim().parse().unwrap_or(0);
                    }
                }
                if headers_done && content_length > 0 {
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body).await;
                    total += content_length;
                }
                let _ = total;
            })
            .await;

            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
        });
        (url, handle)
    }

    #[tokio::test]
    async fn mint_returns_token_on_success() {
        let (url, _h) = spawn_mock_token_endpoint(
            200,
            r#"{"access_token":"ya29.test-token-value","expires_in":3599,"token_type":"Bearer"}"#,
        )
        .await;
        let key = test_sa_key(&url);
        let http = reqwest::Client::new();
        let token = mint("provider-1", &key, "scope-x", &http)
            .await
            .expect("mint ok");
        assert_eq!(token.bearer.expose_secret(), "ya29.test-token-value");
        // expires_at must be in the future
        assert!(token.expires_at > SystemTime::now());
    }

    #[tokio::test]
    async fn mint_403_classifies_permanent() {
        let (url, _h) = spawn_mock_token_endpoint(
            403,
            r#"{"error":"invalid_grant","error_description":"signing key revoked"}"#,
        )
        .await;
        let key = test_sa_key(&url);
        let http = reqwest::Client::new();
        let err = mint("p", &key, "s", &http).await.unwrap_err();
        match err {
            CredentialError::PermanentUpstream { status, body, .. } => {
                assert_eq!(status, 403);
                assert!(body.contains("invalid_grant"));
            }
            other => panic!("expected PermanentUpstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mint_500_classifies_transient() {
        let (url, _h) = spawn_mock_token_endpoint(500, "internal error").await;
        let key = test_sa_key(&url);
        let http = reqwest::Client::new();
        let err = mint("p", &key, "s", &http).await.unwrap_err();
        assert!(
            matches!(err, CredentialError::TransientUpstream { .. }),
            "expected TransientUpstream for 500, got {err:?}"
        );
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn mint_with_unreachable_endpoint_classifies_network() {
        // Port 1 is reserved (TCPMUX) — connection refused on every Linux box.
        let key = test_sa_key("http://127.0.0.1:1/token");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let err = mint("p", &key, "s", &http).await.unwrap_err();
        assert!(
            matches!(err, CredentialError::Network { .. }),
            "expected Network for unreachable endpoint, got {err:?}"
        );
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn mint_with_garbage_pem_returns_signing_failed() {
        let bad_key = Arc::new(GoogleServiceAccountKey {
            client_email: "x@y".to_owned(),
            private_key: RedactedString::new(
                "-----BEGIN PRIVATE KEY-----\nnot-a-real-key\n-----END PRIVATE KEY-----".to_owned(),
            ),
            token_uri: "http://127.0.0.1:1/token".to_owned(),
            project_id: None,
        });
        let http = reqwest::Client::new();
        let err = mint("p", &bad_key, "s", &http).await.unwrap_err();
        assert!(
            matches!(err, CredentialError::SigningFailed { .. }),
            "expected SigningFailed for garbage PEM, got {err:?}"
        );
        assert!(!err.is_retryable());
    }
}
