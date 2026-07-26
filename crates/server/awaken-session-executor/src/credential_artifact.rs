//! Provider-owned credential-file codecs used only at the sandbox provisioning edge.
//!
//! Selection, authorization, revision fencing, and secret lookup remain in
//! `PinnedCredentialMaterializer`. This module only serializes one already-resolved
//! typed material into the fixed format consumed by the pinned provider CLI.

use awaken_runtime_contract::{CredentialMaterial, OAuthCredentialMaterial};
use base64::Engine as _;

pub(crate) struct EncodedCredentialArtifact {
    pub bytes: Vec<u8>,
}

pub(crate) fn relative_path(cli_id: &str) -> Result<&'static str, String> {
    match cli_id {
        "codex" => Ok(".codex/auth.json"),
        "claude" => Ok(".credentials.json"),
        _ => Err(format!("credential_artifact_format_unsupported: {cli_id}")),
    }
}

pub(crate) fn encode(
    cli_id: &str,
    material: CredentialMaterial,
) -> Result<EncodedCredentialArtifact, String> {
    match (cli_id, material) {
        ("codex", CredentialMaterial::Bearer(api_key)) => Ok(EncodedCredentialArtifact {
            bytes: serde_json::to_vec(&serde_json::json!({
                "auth_mode": "apikey",
                "OPENAI_API_KEY": api_key.expose_secret(),
            }))
            .map_err(|_| "credential_artifact_invalid".to_string())?,
        }),
        ("codex", CredentialMaterial::OAuth(bundle)) => encode_codex_oauth(bundle),
        ("claude", CredentialMaterial::OAuth(bundle)) => {
            Ok(EncodedCredentialArtifact {
                bytes: serde_json::to_vec(&serde_json::json!({
                    "claudeAiOauth": {
                        "accessToken": bundle.access_token.expose_secret(),
                        "refreshToken": bundle.refresh_token.expose_secret(),
                        "expiresAt": bundle.expires_at_unix_ms,
                        "scopes": ["user:inference"],
                        "subscriptionType": bundle.account_plan,
                    }
                }))
                .map_err(|_| "credential_artifact_invalid".to_string())?,
            })
        }
        ("claude", CredentialMaterial::Bearer(_)) => Err(
            "credential_material_kind_mismatch: Claude API keys use the existing process-secret delivery"
                .to_string(),
        ),
        _ => Err(format!(
            "credential_artifact_format_unsupported: {cli_id}"
        )),
    }
}

fn encode_codex_oauth(
    bundle: OAuthCredentialMaterial,
) -> Result<EncodedCredentialArtifact, String> {
    // Codex uses the ID token only for local account/plan presentation. Its own
    // upstream fixtures construct an unsigned JWT for the same purpose; provider
    // requests authenticate with `access_token`.
    let header = serde_json::json!({ "alg": "none", "typ": "JWT" });
    let claims = serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": bundle.account_id,
            "chatgpt_plan_type": bundle.account_plan,
        }
    });
    let encode = |value: &serde_json::Value| -> Result<String, String> {
        serde_json::to_vec(value)
            .map(|bytes| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|_| "credential_artifact_invalid".to_string())
    };
    let id_token = format!("{}.{}.c2lnbmF0dXJl", encode(&header)?, encode(&claims)?);
    Ok(EncodedCredentialArtifact {
        bytes: serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token,
                "access_token": bundle.access_token.expose_secret(),
                "refresh_token": bundle.refresh_token.expose_secret(),
                "account_id": bundle.account_id,
            }
        }))
        .map_err(|_| "credential_artifact_invalid".to_string())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;

    fn oauth() -> CredentialMaterial {
        CredentialMaterial::OAuth(OAuthCredentialMaterial {
            access_token: RedactedString::new("access"),
            refresh_token: RedactedString::new("refresh"),
            expires_at_unix_ms: Some(42),
            account_id: Some("account".into()),
            account_plan: Some("pro".into()),
        })
    }

    #[test]
    fn provider_codecs_own_paths_and_never_cross_formats() {
        let codex = encode("codex", oauth()).expect("Codex artifact");
        assert_eq!(relative_path("codex").unwrap(), ".codex/auth.json");
        let codex: serde_json::Value = serde_json::from_slice(&codex.bytes).unwrap();
        assert_eq!(codex["auth_mode"], "chatgpt");
        assert_eq!(codex["tokens"]["access_token"], "access");
        assert_eq!(codex["tokens"]["refresh_token"], "refresh");

        let claude = encode("claude", oauth()).expect("Claude artifact");
        assert_eq!(relative_path("claude").unwrap(), ".credentials.json");
        let claude: serde_json::Value = serde_json::from_slice(&claude.bytes).unwrap();
        assert_eq!(claude["claudeAiOauth"]["accessToken"], "access");
        assert_eq!(claude["claudeAiOauth"]["refreshToken"], "refresh");
    }
}
