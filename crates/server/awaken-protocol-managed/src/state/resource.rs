//! The session-mounted resource cluster (ADR-0038): the neutral
//! [`SessionResource`], its wire parse form, and the DTO projection.

use super::*;

/// One session-mounted resource (ADR-0038), parsed from a wire `resources[]` entry.
/// `kind` is the wire discriminant (`file` / `memory_store` / `github_repository`);
/// `id` is the backing reference (`file_id` / `memory_store_id` / repo `url`);
/// `mount_path` is where it appears in the sandbox; `instructions` is optional
/// per-binding guidance rendered into the system prompt.
#[derive(Debug, Clone)]
pub struct SessionResource {
    pub kind: String,
    pub id: String,
    pub mount_path: String,
    pub instructions: Option<String>,
    /// `github_repository` only: the GitHub PAT the host uses to clone/push. Never
    /// echoed back and never placed in the sandbox (host-side git transport only).
    pub auth_token: Option<String>,
    /// `github_repository` only: the branch to check out (`checkout.name`); `None`
    /// clones the remote's default branch.
    pub git_ref: Option<String>,
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
    // deliberately not consumed — see `into_session_resource`.
    Commit {
        #[allow(dead_code)]
        sha: String,
    },
}

impl WireResource {
    /// Lower to the neutral crate-boundary [`SessionResource`], defaulting the mount
    /// path per kind (mirroring the Managed defaults).
    fn into_session_resource(self) -> SessionResource {
        match self {
            WireResource::File {
                file_id,
                mount_path,
                instructions,
            } => SessionResource {
                kind: "file".into(),
                mount_path: mount_path.unwrap_or_else(|| format!("/mnt/session/uploads/{file_id}")),
                id: file_id,
                instructions,
                auth_token: None,
                git_ref: None,
            },
            WireResource::MemoryStore {
                memory_store_id,
                mount_path,
                instructions,
            } => SessionResource {
                kind: "memory_store".into(),
                mount_path: mount_path.unwrap_or_else(|| "/mnt/memory/store".into()),
                id: memory_store_id,
                instructions,
                auth_token: None,
                git_ref: None,
            },
            WireResource::GithubRepository {
                url,
                mount_path,
                instructions,
                authorization_token,
                checkout,
            } => SessionResource {
                // Repo default mirrors Managed Agents: /workspace/<repo-name>.
                mount_path: mount_path.unwrap_or_else(|| format!("/workspace/{}", repo_name(&url))),
                kind: "github_repository".into(),
                id: url,
                instructions,
                auth_token: authorization_token,
                git_ref: match checkout {
                    Some(WireCheckout::Branch { name }) => Some(name),
                    Some(WireCheckout::Commit { .. }) | None => None,
                },
            },
        }
    }
}

/// Parse one wire `resources[]` entry into a neutral [`SessionResource`]. `None` for
/// a malformed/unknown entry (the caller decides: session-create drops it; the live
/// `resources.add` path turns it into a 400). Shared by both paths.
pub(crate) fn parse_session_resource(v: &serde_json::Value) -> Option<SessionResource> {
    serde_json::from_value::<WireResource>(v.clone())
        .ok()
        .map(WireResource::into_session_resource)
}

/// Project a [`SessionResource`] to an official `BetaManagedAgentsSessionResource`
/// wire entry with a stable id (`{session}:resource:{n}`), so both create-time
/// backfill and live `resources.add` emit an SDK-decodable, uniformly-addressable
/// resource. The auth token is never echoed.
pub(crate) fn resource_dto(session_id: &str, n: usize, res: &SessionResource) -> serde_json::Value {
    use serde_json::json;
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(format!("{session_id}:resource:{n}")));
    obj.insert("type".into(), json!(res.kind));
    obj.insert("mount_path".into(), json!(res.mount_path));
    obj.insert("created_at".into(), json!(PROCESSED_AT));
    obj.insert("updated_at".into(), json!(PROCESSED_AT));
    match res.kind.as_str() {
        "file" => {
            obj.insert("file_id".into(), json!(res.id));
        }
        "memory_store" => {
            obj.insert("memory_store_id".into(), json!(res.id));
            if let Some(i) = &res.instructions {
                obj.insert("instructions".into(), json!(i));
            }
        }
        "github_repository" => {
            obj.insert("url".into(), json!(res.id));
            if let Some(r) = &res.git_ref {
                obj.insert("checkout".into(), json!({ "type": "branch", "name": r }));
            }
        }
        _ => {}
    }
    serde_json::Value::Object(obj)
}
