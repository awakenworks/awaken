//! OAuth access-token acquisition (ADR-0043 Phase 3, feature `oauth-command`).
//!
//! An OAuth-backed provider credential's secret is a **short-lived Bearer token**
//! that must be *refreshed*, not stored once. This module owns that refresh. The
//! materialized token is a [`RedactedString`] used at the injection seam exactly
//! like any other resolved secret (D6/D9) — the difference is only in how it is
//! obtained.
//!
//! [`CommandTokenSource`] delegates the refresh to an external helper whose stdout
//! is the access token — e.g. `gcloud auth print-access-token`, which holds the
//! long-lived Google grant and mints a fresh cloud-platform-scoped token on demand.
//! This is the faithful integration for gcloud-managed Google credentials (used to
//! call Gemini on Vertex AI): the refresh token never enters this process; each
//! call yields a fresh, expiring access token.

use awaken_agent_contract::RedactedString;

use crate::CredentialError;

/// A source of fresh OAuth access tokens. Each call performs (or delegates) a
/// refresh, so the returned token is current.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn access_token(&self) -> Result<RedactedString, CredentialError>;
}

/// Refresh by running an external command whose trimmed stdout is the access
/// token. The command is the OAuth helper that holds the long-lived grant.
pub struct CommandTokenSource {
    program: String,
    args: Vec<String>,
}

impl CommandTokenSource {
    /// A refresher that runs `program args...` and reads the token from stdout.
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The `gcloud auth print-access-token` refresher — a Google OAuth2 access
    /// token (cloud-platform scope) for the active gcloud account.
    #[must_use]
    pub fn gcloud() -> Self {
        Self::new("gcloud", ["auth", "print-access-token"])
    }
}

#[async_trait::async_trait]
impl TokenSource for CommandTokenSource {
    async fn access_token(&self) -> Result<RedactedString, CredentialError> {
        let output = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .output()
            .await
            .map_err(|e| CredentialError::OAuth(format!("spawn `{}`: {e}", self.program)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CredentialError::OAuth(format!(
                "`{}` exited with {}: {}",
                self.program,
                output.status,
                stderr.trim()
            )));
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(CredentialError::OAuth(
                "refresh returned an empty token".into(),
            ));
        }
        Ok(RedactedString::new(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_stdout_becomes_the_token() {
        // A hermetic stand-in for the OAuth helper: `printf` emits a fixed token.
        let source = CommandTokenSource::new("printf", ["ya29.test-token"]);
        let token = source.access_token().await.unwrap();
        assert_eq!(token.expose_secret(), "ya29.test-token");
    }

    #[tokio::test]
    async fn a_failing_helper_is_an_oauth_error() {
        let source = CommandTokenSource::new("false", Vec::<String>::new());
        assert!(matches!(
            source.access_token().await,
            Err(CredentialError::OAuth(_))
        ));
    }
}
