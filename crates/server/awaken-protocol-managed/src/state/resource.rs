//! Managed wire parsing and projection for typed Session inputs.

use super::*;

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireAccess {
    ReadOnly,
    ReadWrite,
}

impl From<WireAccess> for awaken_resource_contract::ResourceAccess {
    fn from(value: WireAccess) -> Self {
        match value {
            WireAccess::ReadOnly => Self::ReadOnly,
            WireAccess::ReadWrite => Self::ReadWrite,
        }
    }
}

/// The repo name for a default mount path: the URL's last path segment, minus a
/// trailing `.git`. Falls back to `repo` when the URL has no usable segment.
fn repo_name(url: &str) -> String {
    let stem = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .strip_suffix(".git")
        .or_else(|| Some(url.trim_end_matches('/').rsplit('/').next().unwrap_or("")))
        .unwrap_or("");
    if stem.is_empty() {
        "repo".to_string()
    } else {
        stem.to_string()
    }
}

/// A wire `resources[]` entry — the official `BetaManagedAgents` resource union,
/// tagged by `type`. Unknown fields are ignored (tolerant of the full SDK payload);
/// an unknown `type` is a deserialize error (fail closed), never a silent drop.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireResource {
    File {
        file_id: String,
        #[serde(default)]
        mount_path: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
    },
    MemoryStore {
        memory_store_id: String,
        #[serde(default)]
        mount_path: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
        #[serde(default)]
        access: Option<WireAccess>,
    },
    GithubRepository {
        url: String,
        #[serde(default)]
        mount_path: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
        #[serde(default)]
        authorization_token: Option<String>,
        #[serde(default)]
        checkout: Option<WireCheckout>,
    },
}

/// A `github_repository` checkout selector. Only `branch` maps to a git ref today
/// (a `commit` sha clones the default branch, matching the prior behavior).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireCheckout {
    Branch {
        name: String,
    },
    // Accepted so the full SDK payload deserializes, but not yet wired to the clone
    // (the host checks out a branch ref; a `sha` clones the default branch). Parsed,
    // deliberately not consumed — mutable repositories are not commit-pinned.
    Commit {
        #[allow(dead_code)]
        sha: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum ParsedInputTarget {
    File(awaken_resource_contract::FileId),
    MemoryStore(awaken_resource_contract::MemoryStoreId),
    Repository {
        remote_url: String,
        authorization_token: Option<String>,
        initial_branch: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedSessionInput {
    pub target: ParsedInputTarget,
    pub mount_path: String,
    pub access: awaken_resource_contract::ResourceAccess,
    pub instructions: Option<String>,
}

impl WireResource {
    fn into_parsed_input(self) -> ParsedSessionInput {
        match self {
            WireResource::File {
                file_id,
                mount_path,
                instructions,
            } => ParsedSessionInput {
                target: ParsedInputTarget::File(file_id.clone().into()),
                mount_path: mount_path.unwrap_or_else(|| format!("/mnt/session/uploads/{file_id}")),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions,
            },
            WireResource::MemoryStore {
                memory_store_id,
                mount_path,
                instructions,
                access,
            } => ParsedSessionInput {
                target: ParsedInputTarget::MemoryStore(memory_store_id.into()),
                mount_path: mount_path.unwrap_or_else(|| "/mnt/memory/store".into()),
                access: access.unwrap_or(WireAccess::ReadWrite).into(),
                instructions,
            },
            WireResource::GithubRepository {
                url,
                mount_path,
                instructions,
                authorization_token,
                checkout,
            } => ParsedSessionInput {
                mount_path: mount_path.unwrap_or_else(|| format!("/workspace/{}", repo_name(&url))),
                target: ParsedInputTarget::Repository {
                    remote_url: url,
                    authorization_token,
                    initial_branch: match checkout {
                        Some(WireCheckout::Branch { name }) => Some(name),
                        Some(WireCheckout::Commit { .. }) | None => None,
                    },
                },
                access: awaken_resource_contract::ResourceAccess::ReadWrite,
                instructions,
            },
        }
    }
}

pub(crate) fn parse_session_input(v: &serde_json::Value) -> Option<ParsedSessionInput> {
    serde_json::from_value::<WireResource>(v.clone())
        .ok()
        .map(WireResource::into_parsed_input)
}

/// Lower a parsed Managed resource into the shared typed binding language. The
/// caller supplies the platform id for a compatibility Repository after it has
/// created that Session-scoped catalog definition.
pub(crate) fn input_binding(
    binding_id: String,
    input: &ParsedSessionInput,
    repository_id: Option<awaken_resource_contract::RepositoryId>,
) -> awaken_resource_contract::InputBinding {
    use awaken_resource_contract::{BindingId, InputBinding, InputResourceId};

    let target = match &input.target {
        ParsedInputTarget::File(file_id) => InputResourceId::File(file_id.clone()),
        ParsedInputTarget::MemoryStore(memory_store_id) => {
            InputResourceId::MemoryStore(memory_store_id.clone())
        }
        ParsedInputTarget::Repository { .. } => InputResourceId::Repository(
            repository_id.expect("Managed Repository lowering supplies a platform id"),
        ),
    };
    InputBinding {
        binding_id: BindingId::new(binding_id),
        target,
        mount_path: input.mount_path.clone(),
        access: input.access,
        instructions: input.instructions.clone(),
    }
}

/// Project one frozen resolved input back to the Managed Session wire. Internal
/// config versions remain an awaken implementation detail; credentials are never
/// echoed.
pub(crate) fn resolved_resource_dto(
    session_id: &str,
    input: &awaken_session_contract::ResolvedInput,
) -> serde_json::Value {
    use awaken_session_contract::ResolvedInputSource;
    use serde_json::json;

    let mut obj = serde_json::Map::new();
    obj.insert(
        "id".into(),
        json!(format!(
            "{session_id}:resource:{}",
            input.binding_id.as_str()
        )),
    );
    obj.insert("mount_path".into(), json!(input.mount_path));
    obj.insert("created_at".into(), json!(PROCESSED_AT));
    obj.insert("updated_at".into(), json!(PROCESSED_AT));
    match &input.source {
        ResolvedInputSource::File { file_id } => {
            obj.insert("type".into(), json!("file"));
            obj.insert("file_id".into(), json!(file_id.as_str()));
        }
        ResolvedInputSource::MemoryStore {
            memory_store_id, ..
        } => {
            obj.insert("type".into(), json!("memory_store"));
            obj.insert("memory_store_id".into(), json!(memory_store_id.as_str()));
            obj.insert(
                "access".into(),
                json!(match input.access {
                    awaken_resource_contract::ResourceAccess::ReadOnly => "read_only",
                    awaken_resource_contract::ResourceAccess::ReadWrite => "read_write",
                }),
            );
            if let Some(instructions) = &input.instructions {
                obj.insert("instructions".into(), json!(instructions));
            }
        }
        ResolvedInputSource::Repository { config, .. } => {
            obj.insert("type".into(), json!("github_repository"));
            obj.insert("url".into(), json!(config.remote_url));
            if let Some(branch) = &config.initial_branch {
                obj.insert(
                    "checkout".into(),
                    json!({ "type": "branch", "name": branch }),
                );
            }
        }
    }
    serde_json::Value::Object(obj)
}
