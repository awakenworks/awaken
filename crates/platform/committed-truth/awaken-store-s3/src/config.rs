//! Object transport configuration; credentials come from the standard AWS chain.
use crate::{Error, Limits, Result, S3Storage, SessionKey};
use std::sync::Arc;
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub bucket: String,
    pub prefix: String,
    pub default_scope: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub allow_http: bool,
}
impl S3Config {
    pub fn validate(&self) -> Result<()> {
        SessionKey::new(&self.default_scope, "_")?;
        if self.bucket.trim().is_empty() || self.prefix.trim_matches('/').is_empty() {
            return Err(Error::Invalid(
                "S3 bucket and isolated prefix are required".into(),
            ));
        }
        if self.endpoint.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Err(Error::Invalid(
                "S3 endpoint must be nonempty when specified".into(),
            ));
        }
        Ok(())
    }
    pub fn open(&self, clock: Arc<dyn awaken_time::WallClock>) -> Result<S3Storage> {
        self.validate()?;
        let transport = awaken_object_storage::s3_transport(
            &self.bucket,
            self.region.as_deref(),
            self.endpoint.as_deref(),
            self.allow_http,
        )
        .map_err(|error| Error::Invalid(error.to_string()))?;
        S3Storage::new(transport, &self.prefix, clock, Limits::default())
    }
}
