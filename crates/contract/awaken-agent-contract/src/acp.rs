use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Adapter-native ACP Session intent attached to an Agent model selection.
///
/// Values remain strings because the selected ACP runtime's negotiated
/// capability descriptor is the authority that validates supported modes and
/// option values at publication time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AcpSessionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// Portable path relative to the Session workspace. The host maps it to the
    /// environment's interior root before sending ACP `session/new.cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
}

impl AcpSessionConfiguration {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mode.is_none() && self.options.is_empty() && self.working_directory.is_none()
    }

    pub fn validate_working_directory(&self) -> Result<(), &'static str> {
        let Some(path) = self.working_directory.as_deref() else {
            return Ok(());
        };
        if path.is_empty() || path.len() > 512 {
            return Err("ACP working_directory must contain 1..=512 characters");
        }
        if path.starts_with('/')
            || path.starts_with('\\')
            || path.contains('\\')
            || path.contains(':')
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(
                "ACP working_directory must be a portable relative path below the Session workspace",
            );
        }
        Ok(())
    }
}
