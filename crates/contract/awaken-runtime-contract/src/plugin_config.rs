use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfigValidation {
    pub accepted: bool,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("unknown plugin config key: {0}")]
    UnknownKey(String),
    #[error("invalid plugin config: {0}")]
    Invalid(String),
}

pub trait PluginConfigValidator {
    fn validate_plugin_config(
        &self,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<PluginConfigValidation, Error>;
}
