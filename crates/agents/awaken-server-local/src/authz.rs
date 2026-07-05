//! Embedded IAM for the management plane (ADR-0042/0043 P1): bearer `ApiToken`
//! authn + grant-based authz gating `/v1/config/*` and `/v1/vaults/*`.
//!
//! # Trust model
//!
//! Opt-in via `AWAKEN_MGMT_IAM=embedded` (requires `AWAKEN_MGMT_DIR`); the
//! default — the variable unset — is today's open single-machine behavior,
//! byte-identical. When enabled, every management route demands a bearer
//! credential in the Anthropic `sk-ant-<prefix>.<secret>` shape (either
//! `Authorization: Bearer …` or the SDK's `x-api-key` header). Secrets are
//! argon2id-hashed at rest by `awaken-iam-core`; the cleartext exists only in
//! the mint response and — for the bootstrap admin token — in
//! `<AWAKEN_MGMT_DIR>/admin-token` (mode 0600).
//!
//! **Bootstrap contract.** On first boot over an empty token directory a
//! single `admin`-role token is minted for the service principal
//! `mgmt-bootstrap` in workspace `wrkspc_default`, logged once to stderr with
//! a rotate-me warning, and written to `<dir>/admin-token`. That file is the
//! single-machine operator hand-off; rotate by revoking/deleting `iam.sqlite`
//! rows or minting successors through [`ManagementAuthz::mint_service_token`]
//! from an embedding. P1 deliberately ships no HTTP mint/revoke surface.
//!
//! **Evaluation.** Authority flows solely from the preset Anthropic role
//! catalog ([`awaken_iam_preset::named_role_catalog`]): each role's action
//! patterns are installed as `GrantSubject::Role` grants at `Global` scope, so
//! a token's reach is confined by its principal's *workspace-scoped*
//! `RoleBinding` (written at mint). Requests are evaluated by the same
//! default-deny [`PolicySet`] engine `awaken-iam-server`'s `AuthzApi` wraps —
//! decisions are byte-identical to a remote `POST /v1/authorize` over the same
//! policy. `RequireApproval` (possible only if a deployment installs such a
//! grant) maps to 403 with a distinct message: P1 has no approval flow.
//!
//! **Persistence + hydration.** The evaluator state (token directory + policy)
//! is in-memory and rebuilt on every boot: role grants are re-derived from the
//! preset catalog, while the durable rows — token records (hash only) and role
//! bindings — hydrate from `<dir>/iam.sqlite`. Every mint writes BOTH the live
//! engine and the SQLite rows under one lock, so a restart over the same
//! directory authenticates previously minted tokens. The store is server-local
//! private (`iam_*` tables, idempotent DDL); `awaken-iam-server`'s SqlStore is
//! not used because its mandatory rusqlite 0.40 cannot share a graph with the
//! workspace's rusqlite 0.32 (`links = "sqlite3"`).
//!
//! **What P1 defers**: an operator mint/revoke HTTP surface, custom roles,
//! org-level scopes, group rosters, entitlements, and approval discharge.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_iam_contract::{
    ActionKey, ApiToken, ApiTokenId, AuthorizationDecision, AuthorizationRequest, PrincipalRef,
    ScopeRef, Timestamp, WorkspaceId,
};
use awaken_iam_core::{
    ApiTokenDirectory, ApiTokenMinter, Effect, Grant, GrantId, GrantSubject, IamError, OsEntropy,
    PolicySet, RoleBinding, RoleId,
};
use awaken_iam_preset::named_role_catalog;
use awaken_protocol_managed::dto::ErrorResponse;
use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Name of the bootstrap admin-token file under the management directory.
pub const ADMIN_TOKEN_FILE: &str = "admin-token";

/// Workspace the bootstrap admin token is bound to.
pub const BOOTSTRAP_WORKSPACE: &str = "wrkspc_default";

/// Service principal id of the bootstrap admin token.
pub const BOOTSTRAP_PRINCIPAL: &str = "mgmt-bootstrap";

/// Largest management request body the guard will buffer to fence its
/// `workspace_id`. Matches axum's own default body limit, so the guard never
/// rejects a body a handler would have accepted.
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// The management plane's action vocabulary, chosen from the namespaces the
/// preset Anthropic roles actually grant (`workspace.*` / `apikey.*`): catalog
/// and admin-aggregate surfaces are workspace configuration; credential, pool,
/// and vault surfaces are API-credential material. So `admin` and
/// `workspace_admin` pass everything, `workspace_restricted_developer` reads
/// everything but writes nothing, and `workspace_user` (no `apikey` patterns)
/// cannot even read credentials.
const WORKSPACE_READ: &str = "workspace.read";
const WORKSPACE_WRITE: &str = "workspace.write";
const APIKEY_READ: &str = "apikey.read";
const APIKEY_WRITE: &str = "apikey.write";

/// The live evaluator state: the argon2-verifying token directory and the
/// default-deny policy (role grants + workspace-scoped bindings). Both are
/// in-memory; [`ManagementAuthz`] keeps them consistent with the SQLite rows.
struct EngineState {
    directory: ApiTokenDirectory,
    policy: PolicySet,
}

/// The embedded management-plane authorizer: authn (bearer token → principal)
/// and authz (principal × action × workspace scope → decision), with its
/// durable token/binding rows in `<dir>/iam.sqlite`.
pub struct ManagementAuthz {
    /// Evaluator state; also serializes mint (engine write + row write) so a
    /// concurrent mint cannot interleave the two.
    state: Mutex<EngineState>,
    conn: Mutex<rusqlite::Connection>,
}

/// A mint request for a workspace-scoped service token (operator embeddings
/// and tests; P1 exposes no HTTP mint surface).
pub struct TokenSpec {
    /// Stable token row id (must be unique).
    pub token_id: String,
    /// Service principal the token authenticates as.
    pub service_id: String,
    /// Workspace the token is bound to; its role binding is written here.
    pub workspace_id: String,
    /// Preset role the principal holds at the workspace (e.g. `admin`,
    /// `workspace_admin`, `workspace_restricted_developer`, `workspace_user`).
    pub role: String,
    /// RFC 3339 mint timestamp; defaults to now.
    pub created_at: Option<String>,
    /// Optional RFC 3339 expiry (strictly after `created_at`).
    pub expires_at: Option<String>,
}

impl ManagementAuthz {
    /// Mint a token: engine write (directory + workspace role binding) and the
    /// durable rows, atomically under the state lock. Returns the one-time
    /// cleartext `sk-ant-…` credential.
    pub fn mint_service_token(&self, spec: TokenSpec) -> Result<String, String> {
        let principal = PrincipalRef::Service {
            service_id: spec.service_id,
        };
        let request = awaken_iam_core::MintApiToken {
            id: ApiTokenId(spec.token_id),
            principal: principal.clone(),
            workspace: WorkspaceId(spec.workspace_id.clone()),
            role: RoleId(spec.role.clone()),
            created_at: Timestamp(spec.created_at.unwrap_or_else(now_rfc3339)),
            expires_at: spec.expires_at.map(Timestamp),
        };
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        let issued = ApiTokenMinter::new(OsEntropy)
            .mint(&mut state.directory, &mut state.policy, request)
            .map_err(|err| err.to_string())?;
        persist_token(
            &self.conn.lock().unwrap(),
            &issued.token,
            &principal,
            &spec.role,
            &spec.workspace_id,
        )?;
        Ok(issued.secret)
    }

    /// Authenticate a presented bearer credential, returning its principal and
    /// workspace binding.
    fn authenticate(&self, presented: &str) -> Result<(PrincipalRef, WorkspaceId), IamError> {
        let state = self.state.lock().unwrap();
        let token = state
            .directory
            .authenticate(presented, &Timestamp(now_rfc3339()))?;
        Ok((token.principal.clone(), token.workspace.clone()))
    }

    /// Evaluate `principal` performing `action` at the token's workspace scope.
    fn authorize(
        &self,
        principal: PrincipalRef,
        action: &str,
        workspace: WorkspaceId,
    ) -> AuthorizationDecision {
        let request = AuthorizationRequest::direct(
            principal,
            ActionKey(action.to_string()),
            ScopeRef::Workspace {
                workspace_id: workspace,
            },
        );
        self.state
            .lock()
            .unwrap()
            .policy
            .evaluate(&request)
            .decision
    }
}

/// Idempotent DDL for the embedded IAM rows. Token records carry the full
/// serde of the contract [`ApiToken`] (argon2id hash, never a secret) in
/// `data`; keyed columns exist only for uniqueness.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS iam_api_token (
    id     TEXT PRIMARY KEY,
    prefix TEXT NOT NULL UNIQUE,
    data   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS iam_role_binding (
    principal TEXT NOT NULL,
    role      TEXT NOT NULL,
    workspace TEXT NOT NULL,
    PRIMARY KEY (principal, role, workspace)
);
";

/// Open (or create) the embedded IAM state under `dir`: migrate
/// `<dir>/iam.sqlite`, install the preset role catalog as role grants, hydrate
/// persisted tokens + bindings into the live evaluator, and — when the token
/// directory is empty — mint the bootstrap admin token (stderr + `<dir>/admin-token`).
///
/// Panics on open/migrate failure, like the durable-store boot path: a
/// management server that silently came up open would be worse than one that
/// refuses to start.
pub fn embedded_iam(dir: &Path) -> Arc<ManagementAuthz> {
    std::fs::create_dir_all(dir).expect("create AWAKEN_MGMT_DIR for embedded IAM");
    let conn = rusqlite::Connection::open(dir.join("iam.sqlite"))
        .expect("open iam.sqlite under AWAKEN_MGMT_DIR");
    conn.execute_batch(SCHEMA).expect("migrate iam.sqlite");

    let mut directory = ApiTokenDirectory::new();
    let mut policy = PolicySet::new();

    // The preset Anthropic role catalog, as data: every role's action patterns
    // become `GrantSubject::Role` grants at Global scope. Global here is NOT a
    // wildcard of authority — a principal only *holds* a role where its
    // workspace-scoped RoleBinding covers, so the binding confines the reach.
    // Re-derived every boot (roles are seed data; custom roles are post-P1).
    let now = Timestamp(now_rfc3339());
    for role in named_role_catalog(&now) {
        for (index, pattern) in role.action_patterns.iter().enumerate() {
            policy.add_grant(Grant {
                id: GrantId(format!("role:{}:{index}", role.id.0)),
                subject: GrantSubject::Role(role.id.clone()),
                action_pattern: pattern.clone(),
                scope: ScopeRef::Global,
                effect: Effect::Allow,
            });
        }
    }

    // Hydrate the durable rows into the in-memory evaluator (it is never
    // auto-hydrated): token records into the directory, bindings into the
    // policy. Failing closed on a corrupt row (panic) beats dropping a token
    // silently — the operator would see 401s with no way to tell why.
    let mut hydrated_tokens = 0usize;
    {
        let mut stmt = conn
            .prepare("SELECT data FROM iam_api_token ORDER BY id")
            .expect("prepare token hydration");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query token rows");
        for data in rows {
            let token: ApiToken = serde_json::from_str(&data.expect("read token row"))
                .expect("decode persisted ApiToken row");
            directory.create(token).expect("hydrate unique token row");
            hydrated_tokens += 1;
        }
        let mut stmt = conn
            .prepare("SELECT principal, role, workspace FROM iam_role_binding")
            .expect("prepare binding hydration");
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("query binding rows");
        for row in rows {
            let (principal, role, workspace) = row.expect("read binding row");
            policy.bind_role(RoleBinding {
                principal: serde_json::from_str(&principal)
                    .expect("decode persisted principal ref"),
                role: RoleId(role),
                scope: ScopeRef::Workspace {
                    workspace_id: WorkspaceId(workspace),
                },
            });
        }
    }

    let authz = Arc::new(ManagementAuthz {
        state: Mutex::new(EngineState { directory, policy }),
        conn: Mutex::new(conn),
    });

    if hydrated_tokens == 0 {
        bootstrap_admin_token(&authz, dir);
    }
    authz
}

/// First-boot bootstrap: mint the one `admin`-role token the operator starts
/// from, log its cleartext once to stderr, and write it to `<dir>/admin-token`
/// (mode 0600). Single-machine hand-off by design (P1); rotate it early.
fn bootstrap_admin_token(authz: &ManagementAuthz, dir: &Path) {
    let secret = authz
        .mint_service_token(TokenSpec {
            token_id: "tok_mgmt_bootstrap".to_string(),
            service_id: BOOTSTRAP_PRINCIPAL.to_string(),
            workspace_id: BOOTSTRAP_WORKSPACE.to_string(),
            role: "admin".to_string(),
            created_at: None,
            expires_at: None,
        })
        .expect("mint the bootstrap admin token");
    let path = dir.join(ADMIN_TOKEN_FILE);
    write_owner_only(&path, &secret).expect("write the bootstrap admin-token file");
    eprintln!(
        "awaken-server-local: EMBEDDED IAM BOOTSTRAP — minted the admin API token \
         for principal `{BOOTSTRAP_PRINCIPAL}` in workspace `{BOOTSTRAP_WORKSPACE}`.\n\
         It is printed ONCE and written to {} (mode 0600).\n\
         ROTATE IT: anyone holding this token has full management authority.\n\
         {secret}",
        path.display()
    );
}

/// Write `contents` to `path` readable by the owner only (0600 on unix).
fn write_owner_only(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

/// Persist a freshly minted token + its workspace role binding. Called with
/// the engine lock held so row and engine cannot diverge under concurrency.
fn persist_token(
    conn: &rusqlite::Connection,
    token: &ApiToken,
    principal: &PrincipalRef,
    role: &str,
    workspace: &str,
) -> Result<(), String> {
    let data = serde_json::to_string(token).map_err(|err| err.to_string())?;
    conn.execute(
        "INSERT INTO iam_api_token (id, prefix, data) VALUES (?1, ?2, ?3)",
        rusqlite::params![token.id.0, token.prefix.0, data],
    )
    .map_err(|err| format!("persist token row: {err}"))?;
    let principal = serde_json::to_string(principal).map_err(|err| err.to_string())?;
    conn.execute(
        "INSERT OR IGNORE INTO iam_role_binding (principal, role, workspace) VALUES (?1, ?2, ?3)",
        rusqlite::params![principal, role, workspace],
    )
    .map_err(|err| format!("persist role binding: {err}"))?;
    Ok(())
}

// ---- Middleware --------------------------------------------------------------

/// The management-plane guard: authenticate the bearer token, fence any
/// `workspace_id` the request names against the token's workspace, map the
/// route to its management action, and authorize at the token's workspace
/// scope. 401 (`authentication_error`) for a missing/invalid/expired/revoked
/// token; 403 (`permission_error`) for a denied or approval-gated action or a
/// workspace mismatch — all in the Managed wire's [`ErrorResponse`] envelope so
/// SDK clients parse them.
pub async fn management_guard(
    State(authz): State<Arc<ManagementAuthz>>,
    req: Request,
    next: Next,
) -> Response {
    // Fail closed on an unmapped route: the guard only wraps the management
    // routers, so reaching this arm means a route was added without extending
    // the action table.
    let Some(action) = action_for(req.method(), req.uri().path()) else {
        return forbidden("no management action is mapped for this route");
    };

    let Some(presented) = bearer_token(req.headers()) else {
        return unauthorized("missing management API token (Authorization: Bearer or x-api-key)");
    };
    let (principal, workspace) = match authz.authenticate(&presented) {
        Ok(identity) => identity,
        Err(IamError::ApiTokenExpired { .. }) => return unauthorized("API token is expired"),
        Err(IamError::ApiTokenRevoked { .. }) => return unauthorized("API token is revoked"),
        Err(_) => return unauthorized("invalid API token"),
    };

    // Workspace fence: wherever the request names a workspace_id (query string
    // or a top-level JSON body field), it must equal the token's workspace.
    if let Some(named) = query_workspace_id(req.uri().query())
        && named != workspace.0
    {
        return forbidden("workspace_id does not match the API token's workspace");
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "management request body exceeds the guard's buffer limit",
                )),
            )
                .into_response();
        }
    };
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_slice(&bytes)
        && let Some(serde_json::Value::String(named)) = map.get("workspace_id")
        && *named != workspace.0
    {
        return forbidden("workspace_id does not match the API token's workspace");
    }
    let req = Request::from_parts(parts, Body::from(bytes));

    match authz.authorize(principal, action, workspace) {
        AuthorizationDecision::Allow => next.run(req).await,
        // P1 has no approval flow to discharge the obligation, so an
        // approval-gated action is refused with its own message (documented).
        AuthorizationDecision::RequireApproval => {
            forbidden("this action requires approval, which the embedded P1 plane cannot grant")
        }
        AuthorizationDecision::Deny => {
            forbidden("the API token's role does not authorize this action")
        }
    }
}

/// The presented credential: `Authorization: Bearer <token>` (the documented
/// form) or `x-api-key: <token>` (what the Anthropic SDK sends for `apiKey`).
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(value) = value.to_str()
    {
        let mut parts = value.splitn(2, ' ');
        if let (Some(scheme), Some(token)) = (parts.next(), parts.next())
            && scheme.eq_ignore_ascii_case("bearer")
            && !token.trim().is_empty()
        {
            return Some(token.trim().to_string());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
}

/// `workspace_id` from a query string, decoding only the characters the
/// management plane's ids use (they are plain `[A-Za-z0-9:_-]`, never
/// percent-encoded by our SDKs; an encoded exotic id simply fails the fence,
/// which is the closed direction).
fn query_workspace_id(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "workspace_id")
        .map(|(_, value)| value.replace('+', " "))
}

/// Map a management route + method to the action it requires. Reads (GET) map
/// to `*.read`; every mutation maps to a write action. Credential, pool, and
/// vault surfaces live under `apikey.*`; catalog and admin aggregates under
/// `workspace.*` (see the constants above for the role-fit rationale).
fn action_for(method: &Method, path: &str) -> Option<&'static str> {
    let read = *method == Method::GET;
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        // -- catalog (providers / endpoints / offerings / snapshot) --
        ["v1", "config", "catalog"] if read => Some(WORKSPACE_READ),
        ["v1", "config", "providers", _] | ["v1", "config", "endpoints", _] => Some(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "offerings"] if !read => Some(WORKSPACE_WRITE),
        // -- credentials + pools --
        ["v1", "config", "credentials"] => Some(if read { APIKEY_READ } else { APIKEY_WRITE }),
        ["v1", "config", "credentials", _] if read => Some(APIKEY_READ),
        ["v1", "config", "credentials", _, "archive" | "validate"] if !read => Some(APIKEY_WRITE),
        ["v1", "config", "credential-pools", _] => {
            Some(if read { APIKEY_READ } else { APIKEY_WRITE })
        }
        // -- inference profiles + resolve dry-runs (secret-free projections) --
        ["v1", "config", "inference-profiles", _] => Some(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "inference-profiles", _, "resolve"] if !read => Some(WORKSPACE_READ),
        ["v1", "config", "inference", "resolve"] if !read => Some(WORKSPACE_READ),
        // -- MCP server defs + agent bindings --
        ["v1", "config", "mcp-servers"] if read => Some(WORKSPACE_READ),
        ["v1", "config", "mcp-servers", _] => Some(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "agents", _, "mcp"] => Some(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "agents", _, "mcp", "resolve"] if !read => Some(WORKSPACE_READ),
        // -- the Managed vault front door --
        ["v1", "vaults"] if !read => Some(APIKEY_WRITE),
        ["v1", "vaults", _] => Some(if read { APIKEY_READ } else { APIKEY_WRITE }),
        ["v1", "vaults", _, "credentials"] if !read => Some(APIKEY_WRITE),
        ["v1", "vaults", _, "credentials", _] if read => Some(APIKEY_READ),
        ["v1", "vaults", _, "credentials", _, "mcp_oauth_validate"] if !read => Some(APIKEY_WRITE),
        _ => None,
    }
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse::new("authentication_error", message)),
    )
        .into_response()
}

fn forbidden(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse::new("permission_error", message)),
    )
        .into_response()
}

// ---- Time ---------------------------------------------------------------------

/// Now as a canonical RFC 3339 UTC string (the contract's `Timestamp` shape,
/// whose lexical order is chronological order). Hand-rolled civil-from-days so
/// the assembly does not grow a date-time dependency for one format call.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's public-domain
/// `civil_from_days` algorithm, exact for the whole proleptic Gregorian range.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_table_maps_reads_to_read_actions_and_mutations_to_writes() {
        let get = Method::GET;
        let post = Method::POST;
        let put = Method::PUT;
        let delete = Method::DELETE;
        assert_eq!(action_for(&get, "/v1/config/catalog"), Some(WORKSPACE_READ));
        assert_eq!(
            action_for(&put, "/v1/config/providers/anthropic"),
            Some(WORKSPACE_WRITE)
        );
        assert_eq!(
            action_for(&get, "/v1/config/providers/anthropic"),
            Some(WORKSPACE_READ)
        );
        assert_eq!(
            action_for(&post, "/v1/config/offerings"),
            Some(WORKSPACE_WRITE)
        );
        assert_eq!(
            action_for(&post, "/v1/config/credentials"),
            Some(APIKEY_WRITE)
        );
        assert_eq!(
            action_for(&get, "/v1/config/credentials"),
            Some(APIKEY_READ)
        );
        assert_eq!(
            action_for(&post, "/v1/config/credentials/c1/archive"),
            Some(APIKEY_WRITE)
        );
        assert_eq!(
            action_for(&post, "/v1/config/credentials/c1/validate"),
            Some(APIKEY_WRITE)
        );
        assert_eq!(
            action_for(&put, "/v1/config/credential-pools/p1"),
            Some(APIKEY_WRITE)
        );
        assert_eq!(
            action_for(&post, "/v1/config/inference/resolve"),
            Some(WORKSPACE_READ)
        );
        assert_eq!(
            action_for(&post, "/v1/config/inference-profiles/p/resolve"),
            Some(WORKSPACE_READ)
        );
        assert_eq!(
            action_for(&put, "/v1/config/agents/a1/mcp"),
            Some(WORKSPACE_WRITE)
        );
        assert_eq!(
            action_for(&post, "/v1/config/agents/a1/mcp/resolve"),
            Some(WORKSPACE_READ)
        );
        assert_eq!(action_for(&post, "/v1/vaults"), Some(APIKEY_WRITE));
        assert_eq!(action_for(&get, "/v1/vaults/v1"), Some(APIKEY_READ));
        assert_eq!(action_for(&delete, "/v1/vaults/v1"), Some(APIKEY_WRITE));
        assert_eq!(
            action_for(&post, "/v1/vaults/v1/credentials"),
            Some(APIKEY_WRITE)
        );
        assert_eq!(
            action_for(&post, "/v1/vaults/v1/credentials/c1/mcp_oauth_validate"),
            Some(APIKEY_WRITE)
        );
        // An unmapped route fails closed (the guard turns None into 403).
        assert_eq!(action_for(&get, "/v1/config/unknown"), None);
        assert_eq!(action_for(&post, "/v1/config/catalog"), None);
    }

    #[test]
    fn timestamps_render_canonical_rfc3339_utc() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // leap-year boundary
        assert_eq!(civil_from_days(20_513), (2026, 3, 1)); // day after 2026-02-28
        let now = now_rfc3339();
        assert_eq!(now.len(), 20);
        assert!(now.ends_with('Z'));
        assert!(now.starts_with("20"));
    }

    #[test]
    fn the_query_fence_reads_workspace_id_pairs() {
        assert_eq!(
            query_workspace_id(Some("workspace_id=ws1")),
            Some("ws1".to_string())
        );
        assert_eq!(
            query_workspace_id(Some("a=b&workspace_id=ws2")),
            Some("ws2".to_string())
        );
        assert_eq!(query_workspace_id(Some("a=b")), None);
        assert_eq!(query_workspace_id(None), None);
    }
}
