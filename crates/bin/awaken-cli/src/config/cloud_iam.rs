use std::path::PathBuf;

use super::{FileConfig, OperatingMode};

#[derive(Clone)]
pub struct CloudIamConfig {
    pub base_url: String,
    pub inference_base_url: String,
    pub audience: String,
    pub issuer: String,
    pub oauth_client_id: String,
    pub oauth_redirect_uri: String,
    pub access_token: Option<String>,
    pub developer_key_file: Option<PathBuf>,
    pub service_token: Option<String>,
    pub service_token_file: Option<PathBuf>,
}

impl CloudIamConfig {
    pub(super) fn resolve(
        file: &FileConfig,
        mode: OperatingMode,
        identity_mode: awaken_control::ManagementIdentityMode,
    ) -> Result<Self, String> {
        if file.cloud_iam_service_token.is_some() && file.cloud_iam_service_token_file.is_some() {
            return Err(
                "configure exactly one of cloud_iam_service_token or cloud_iam_service_token_file"
                    .to_owned(),
            );
        }
        if mode == OperatingMode::Server
            && (file.cloud_access_token.is_some() || file.cloud_iam_service_token.is_some())
        {
            return Err(
                "server mode forbids inline Cloud credentials; use projected credential files"
                    .to_owned(),
            );
        }
        if mode == OperatingMode::Server
            && identity_mode == awaken_control::ManagementIdentityMode::AwakenCloud
            && file.cloud_iam_service_token_file.is_none()
        {
            return Err(
                "server-mode Awaken Cloud identity requires cloud_iam_service_token_file"
                    .to_owned(),
            );
        }
        Ok(Self {
            base_url: file
                .cloud_iam_url
                .clone()
                .unwrap_or_else(|| "https://accounts.awakenworks.com".to_owned()),
            inference_base_url: file
                .cloud_api_url
                .clone()
                .unwrap_or_else(|| "https://api.awakenworks.com".to_owned()),
            audience: file
                .cloud_iam_audience
                .clone()
                .unwrap_or_else(|| "awaken-runtime".to_owned()),
            issuer: file
                .cloud_iam_issuer
                .clone()
                .unwrap_or_else(|| "https://accounts.awakenworks.com".to_owned()),
            oauth_client_id: file
                .cloud_oauth_client_id
                .clone()
                .unwrap_or_else(|| "awaken-desktop".to_owned()),
            oauth_redirect_uri: file
                .cloud_oauth_redirect_uri
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:34115/callback".to_owned()),
            access_token: file.cloud_access_token.clone(),
            developer_key_file: file.cloud_api_key_file.clone(),
            service_token: file.cloud_iam_service_token.clone(),
            service_token_file: file.cloud_iam_service_token_file.clone(),
        })
    }
}

impl std::fmt::Debug for CloudIamConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudIamConfig")
            .field("base_url", &self.base_url)
            .field("inference_base_url", &self.inference_base_url)
            .field("audience", &self.audience)
            .field("issuer", &self.issuer)
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_redirect_uri", &self.oauth_redirect_uri)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("developer_key_file", &self.developer_key_file)
            .field(
                "service_token",
                &self.service_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("service_token_file", &self.service_token_file)
            .finish()
    }
}
