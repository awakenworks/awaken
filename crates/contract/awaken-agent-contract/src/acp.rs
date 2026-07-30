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
}

impl AcpSessionConfiguration {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mode.is_none() && self.options.is_empty()
    }
}
