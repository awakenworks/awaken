//! Plugin config validation seam.
//!
//! Per ADR-0004 D3 (amended A1): there is one validation *implementation*,
//! `validate_section`, derived from the typed config. This `PluginConfigValidator`
//! trait is NOT a second validator — it is retained only as the thin
//! runtime↔server DI seam that forwards to `validate_section` (the server depends
//! on this contract trait, not on the concrete runtime). The original ADR text
//! "the separate `PluginConfigValidator` port is removed" is superseded: the
//! duplicate logic collapses, the seam stays.

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
