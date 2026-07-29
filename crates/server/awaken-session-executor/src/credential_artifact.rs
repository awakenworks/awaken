//! Provider-owned credential-file codecs used only at the sandbox provisioning edge.
//!
//! Selection, authorization, revision fencing, and secret lookup remain in
//! `PinnedCredentialMaterializer`. This module only serializes one already-resolved
//! typed material into the fixed format consumed by the pinned provider CLI.

use awaken_run_executor_acp::CredentialArtifactCodec;
use awaken_runtime_contract::{CredentialMaterial, OAuthCredentialMaterial};
use base64::Engine as _;

pub(crate) struct EncodedCredentialArtifact {
    pub bytes: Vec<u8>,
}

pub(crate) fn encode(
    codec: CredentialArtifactCodec,
    material: CredentialMaterial,
) -> Result<EncodedCredentialArtifact, String> {
    match (codec, material) {
        (CredentialArtifactCodec::CodexAuthJson, CredentialMaterial::Secret(api_key)) => {
            Ok(EncodedCredentialArtifact {
                bytes: serde_json::to_vec(&serde_json::json!({
                    "auth_mode": "apikey",
                    "OPENAI_API_KEY": api_key.expose_secret(),
                }))
                .map_err(|_| "credential_artifact_invalid".to_string())?,
            })
        }
        (CredentialArtifactCodec::CodexAuthJson, CredentialMaterial::OAuth(bundle)) => {
            encode_codex_oauth(bundle)
        }
        (CredentialArtifactCodec::CodexAuthJson, CredentialMaterial::Structured(_)) => {
            Err("credential_artifact_material_unsupported".to_string())
        }
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
    fn provider_codecs_follow_the_material_kind_decision_table() {
        // Cause graph: catalog-selected codec + exact material kind -> one
        // provider-owned serialization. A codec never guesses another CLI format.
        //
        // | Rule | Codec | Material | Result |
        // | E1 | Codex | OAuth | ChatGPT auth.json |
        // | E2 | Codex | Bearer | API-key auth.json |
        // Claude managed credentials never use a file artifact: both API keys
        // and setup tokens are exact process-secret environment requirements.
        let codex =
            encode(CredentialArtifactCodec::CodexAuthJson, oauth()).expect("Codex artifact");
        let codex: serde_json::Value = serde_json::from_slice(&codex.bytes).unwrap();
        assert_eq!(codex["auth_mode"], "chatgpt", "E1");
        assert_eq!(codex["tokens"]["access_token"], "access");
        assert_eq!(codex["tokens"]["refresh_token"], "refresh");

        let codex = encode(
            CredentialArtifactCodec::CodexAuthJson,
            CredentialMaterial::secret(RedactedString::new("api-key")),
        )
        .expect("Codex API-key artifact");
        let codex: serde_json::Value = serde_json::from_slice(&codex.bytes).unwrap();
        assert_eq!(codex["auth_mode"], "apikey", "E2");
        assert_eq!(codex["OPENAI_API_KEY"], "api-key", "E2");
    }
}
