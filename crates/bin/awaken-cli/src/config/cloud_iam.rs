use std::path::PathBuf;

#[derive(Clone)]
pub struct CloudIamConfig {
    pub base_url: String,
    pub inference_base_url: String,
    pub audience: String,
    pub issuer: String,
    pub oauth_client_id: String,
    pub oauth_redirect_uri: String,
    pub access_token: Option<String>,
    pub service_token: Option<String>,
    pub service_token_file: Option<PathBuf>,
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
            .field(
                "service_token",
                &self.service_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("service_token_file", &self.service_token_file)
            .finish()
    }
}
