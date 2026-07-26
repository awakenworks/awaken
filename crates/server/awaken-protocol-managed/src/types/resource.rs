//! Typed inputs shared by Session and Deployment `resources[]`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    ReadOnly,
    ReadWrite,
}

/// The official Managed Agents resource union. The raw authorization-token
/// compatibility field is intentionally absent, and unknown fields fail closed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceInput {
    File {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
    },
    MemoryStore {
        memory_store_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access: Option<ResourceAccess>,
    },
    GithubRepository {
        url: String,
        /// Awaken security extension: an opaque, pre-existing Vault binding.
        /// It replaces the official write-only raw `authorization_token` field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_binding: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<RepositoryCheckout>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryCheckout {
    Branch { name: String },
    Commit { sha: String },
}

/// `BetaManagedAgentsSessionResource` — the closed output union returned from
/// Session create/retrieve and the `sessions.resources` family. It is a pure
/// projection of the typed Session aggregate and never a second resource index.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionResource {
    File {
        id: String,
        created_at: &'static str,
        file_id: String,
        mount_path: String,
        updated_at: &'static str,
    },
    GithubRepository {
        id: String,
        created_at: &'static str,
        mount_path: String,
        updated_at: &'static str,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        checkout: Option<RepositoryCheckout>,
    },
    MemoryStore {
        memory_store_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        access: Option<ResourceAccess>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
    },
}

impl SessionResource {
    /// Only File and Repository resources have an official addressable resource
    /// id. Memory stores are immutable create-time attachments in this API.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::File { id, .. } | Self::GithubRepository { id, .. } => Some(id),
            Self::MemoryStore { .. } => None,
        }
    }
}

/// `ResourceAddParams` — the official live-resource endpoint admits only an
/// already-uploaded File. Repository and Memory bindings belong to the Session
/// creation snapshot, so the endpoint cannot create a parallel mutable path.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceAddParams {
    pub file_id: String,
    #[serde(rename = "type")]
    pub kind: FileResourceKind,
    #[serde(default)]
    pub mount_path: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileResourceKind {
    File,
}

impl ResourceAddParams {
    #[must_use]
    pub(crate) fn into_resource_input(self) -> ResourceInput {
        let Self {
            file_id,
            kind: _,
            mount_path,
        } = self;
        ResourceInput::File {
            file_id,
            mount_path,
        }
    }
}

/// The SDK update body contains only raw credential material, which Awaken never
/// admits. Every supplied field fails at this typed admission boundary before
/// state or Runtime effects; `{}` receives a stable unsupported-command error.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUpdateParams {}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeletedSessionResource {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resource_union_rejects_raw_credentials_and_unknown_shapes() {
        // JSON resource -> tagged union -> neutral binding -> sandbox realization
        //
        // | known type | required fields | raw/unknown field | admission |
        // |------------|-----------------|-------------------|-----------|
        // | yes        | yes             | no                | accept    |
        // | yes        | yes             | authorization     | reject    |
        // | yes        | misspelled      | no                | reject    |
        // | no         | any             | any               | reject    |
        let valid = json!({
            "type":"github_repository",
            "url":"https://github.com/acme/repo.git",
            "checkout":{"type":"branch", "name":"main"}
        });
        assert!(serde_json::from_value::<ResourceInput>(valid).is_ok());

        for invalid in [
            json!({
                "type":"github_repository",
                "url":"https://github.com/acme/repo.git",
                "authorization_token":"secret"
            }),
            json!({"type":"file", "id":"file_1"}),
            json!({"type":"vault", "vault_id":"vlt_1"}),
        ] {
            assert!(serde_json::from_value::<ResourceInput>(invalid).is_err());
        }
    }
}
