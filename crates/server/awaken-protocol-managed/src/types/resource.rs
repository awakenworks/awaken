//! Typed inputs shared by Session and Deployment `resources[]`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    ReadOnly,
    ReadWrite,
}

/// The official Managed Agents resource union. The raw authorization-token
/// compatibility field is intentionally absent, and unknown fields fail closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceInput {
    File {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<RepositoryCheckout>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryCheckout {
    Branch { name: String },
    Commit { sha: String },
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
