//! Shared, secret-free language for resources used as Agent or Session inputs.
//!
//! These values describe *what* may be activated and the maximum access of the
//! binding. They deliberately carry no subject, role, policy, API key, decision,
//! host path, live credential, Project, or WorkUnit.

use serde::{Deserialize, Serialize};

macro_rules! resource_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

resource_id!(BindingId);
resource_id!(FileId);
resource_id!(MemoryStoreId);
resource_id!(RepositoryId);

/// The stable resource identity named by an Agent default or Session attachment.
/// Mutable Memory and Repository content is intentionally not represented here.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum InputResourceId {
    File(FileId),
    MemoryStore(MemoryStoreId),
    Repository(RepositoryId),
}

impl InputResourceId {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::File(id) => id.as_str(),
            Self::MemoryStore(id) => id.as_str(),
            Self::Repository(id) => id.as_str(),
        }
    }
}

/// Maximum access carried by a binding. Resource realizers and live policy
/// overlays may narrow it, but no downstream component may widen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    ReadOnly,
    ReadWrite,
}

/// One language shared by Agent defaults and Session attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputBinding {
    pub binding_id: BindingId,
    pub target: InputResourceId,
    /// Sandbox-visible path. It is validated and normalized by the Session input
    /// resolver; it is never an absolute host path.
    pub mount_path: String,
    pub access: ResourceAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_wire_is_typed_and_secret_free() {
        let binding = InputBinding {
            binding_id: BindingId::from("source"),
            target: InputResourceId::Repository(RepositoryId::from("repo-7")),
            mount_path: "/workspace/source".into(),
            access: ResourceAccess::ReadWrite,
            instructions: None,
        };

        let wire = serde_json::to_value(&binding).unwrap();
        assert_eq!(wire["target"]["kind"], "repository");
        assert_eq!(wire["target"]["id"], "repo-7");
        assert!(wire.get("principal").is_none());
        assert!(wire.get("credential").is_none());
        assert_eq!(
            serde_json::from_value::<InputBinding>(wire).unwrap(),
            binding
        );
    }

    #[test]
    fn different_resource_id_kinds_cannot_compare_equal() {
        assert_ne!(
            InputResourceId::File(FileId::from("same")),
            InputResourceId::MemoryStore(MemoryStoreId::from("same"))
        );
    }
}
