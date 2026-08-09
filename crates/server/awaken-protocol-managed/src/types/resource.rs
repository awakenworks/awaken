//! Typed inputs shared by Session and Deployment `resources[]`.

use serde::{Deserialize, Deserializer, Serialize};

/// Write-only repository credential admitted by the Anthropic-compatible wire.
///
/// It is deliberately neither serializable nor printable. The state adapter
/// consumes it into the canonical credential Vault before a Session snapshot is
/// persisted, and the wrapped value is zeroized when this request DTO is dropped.
#[derive(Clone)]
pub struct RepositoryAuthorizationToken(awaken_agent_contract::RedactedString);

impl RepositoryAuthorizationToken {
    #[must_use]
    pub(crate) fn into_redacted(self) -> awaken_agent_contract::RedactedString {
        self.0
    }
}

impl<'de> Deserialize<'de> for RepositoryAuthorizationToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|value| Self(value.into()))
    }
}

impl std::fmt::Debug for RepositoryAuthorizationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RepositoryAuthorizationToken(***)")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    ReadOnly,
    ReadWrite,
}

/// The official Managed Agents resource union. Repository authorization is a
/// write-only ingress field and unknown fields fail closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        /// Official write-only clone credential. Serialization always omits it,
        /// so Session projections and idempotency snapshots cannot echo material.
        #[serde(default, skip_serializing)]
        authorization_token: Option<RepositoryAuthorizationToken>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<RepositoryCheckout>,
    },
}

impl ResourceInput {
    /// One-way request-equivalence fingerprint. Repository credentials remain
    /// absent from every persisted/projection DTO but still participate in
    /// idempotency mismatch detection.
    pub(crate) fn idempotency_fingerprint(&self) -> String {
        match self {
            Self::File {
                file_id,
                mount_path,
            } => awaken_session_contract::stable_fingerprint(&("file", file_id, mount_path)),
            Self::MemoryStore {
                memory_store_id,
                mount_path,
                instructions,
                access,
            } => awaken_session_contract::stable_fingerprint(&(
                "memory_store",
                memory_store_id,
                mount_path,
                instructions,
                access,
            )),
            Self::GithubRepository {
                url,
                authorization_token,
                mount_path,
                checkout,
            } => awaken_session_contract::stable_fingerprint(&(
                "github_repository",
                url,
                authorization_token
                    .as_ref()
                    .map(|token| token.0.expose_secret()),
                mount_path,
                checkout,
            )),
        }
    }
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

/// Official write-only repository credential rotation body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUpdateParams {
    pub authorization_token: RepositoryAuthorizationToken,
}

/// Complete desired Resource set for one Session. Absence from this list is a
/// removal; callers never need to sequence remote delete/add operations.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceManifestReplaceParams {
    pub resources: Vec<ResourceInput>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceManifestPhase {
    Active,
    Applying,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionResourceManifest {
    pub desired_revision: u64,
    pub applied_revision: u64,
    pub phase: ResourceManifestPhase,
    pub resources: Vec<SessionResource>,
}

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
    fn resource_union_accepts_write_only_credentials_and_rejects_unknown_shapes() {
        // JSON resource -> tagged union -> neutral binding -> sandbox realization
        //
        // | known type | required fields | raw/unknown field | admission |
        // |------------|-----------------|-------------------|-----------|
        // | yes        | yes             | no                | accept    |
        // | yes        | yes             | authorization     | accept    |
        // | yes        | misspelled      | no                | reject    |
        // | no         | any             | any               | reject    |
        let valid = json!({
            "type":"github_repository",
            "url":"https://github.com/acme/repo.git",
            "checkout":{"type":"branch", "name":"main"}
        });
        assert!(serde_json::from_value::<ResourceInput>(valid).is_ok());

        let credential = serde_json::from_value::<ResourceInput>(json!({
            "type":"github_repository",
            "url":"https://github.com/acme/repo.git",
            "authorization_token":"secret"
        }))
        .unwrap();
        let serialized = serde_json::to_string(&credential).unwrap();
        assert!(!serialized.contains("authorization_token"));
        assert!(!serialized.contains("secret"));

        for invalid in [
            json!({"type":"github_repository", "url":"https://github.com/acme/repo.git", "credential_binding":"internal"}),
            json!({"type":"file", "id":"file_1"}),
            json!({"type":"vault", "vault_id":"vlt_1"}),
        ] {
            assert!(serde_json::from_value::<ResourceInput>(invalid).is_err());
        }
    }
}
