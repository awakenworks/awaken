//! Durable, provider-neutral runtime incarnation evidence for Sandbox control.

use serde::{Deserialize, Serialize};

use super::SandboxError;

/// Validated Kubernetes Pod UID used as durable incarnation evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct KubernetesPodUid(String);

impl KubernetesPodUid {
    pub fn new(value: impl Into<String>) -> Result<Self, SandboxError> {
        let value = value.into();
        if value.trim().is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
        {
            return Err(SandboxError::new("Kubernetes Pod UID is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for KubernetesPodUid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Provider-neutral durable evidence for the exact runtime object allowed to
/// host a Sandbox control endpoint. The closed enum prevents adapters from
/// inventing unverified opaque identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlIncarnation {
    KubernetesPod { uid: KubernetesPodUid },
}

impl SandboxControlIncarnation {
    pub fn kubernetes_pod(uid: impl Into<String>) -> Result<Self, SandboxError> {
        Ok(Self::KubernetesPod {
            uid: KubernetesPodUid::new(uid)?,
        })
    }

    #[must_use]
    pub fn kubernetes_pod_uid(&self) -> Option<&str> {
        match self {
            Self::KubernetesPod { uid } => Some(uid.as_str()),
        }
    }
}
