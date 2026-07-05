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
//! single-machine operator hand-off; rotate by minting a successor admin token
//! through `POST /v1/config/iam/tokens` and revoking the bootstrap token
//! through `DELETE /v1/config/iam/tokens/{id}` (or from an embedding via
//! [`ManagementAuthz::mint_service_token`]).
//!
//! **Bootstrap scope.** The mint path writes the bootstrap principal's `admin`
//! role binding at `Workspace { wrkspc_default }` — which alone could not
//! administer any *other* workspace, defeating a bootstrap credential. So boot
//! additionally binds the bootstrap principal's `admin` role at
//! [`ScopeRef::Global`]: in the scope graph, `Global` is an ancestor of every
//! workspace scope, so the global binding lets the bootstrap credential
//! provision per-workspace tokens for ANY workspace through the token routes
//! and then be revoked. The binding row persists in `iam.sqlite` (workspace
//! column sentinel [`GLOBAL_BINDING_WORKSPACE`]) and is ensured idempotently on
//! every boot, so existing installs bootstrapped before this fix gain it on
//! their next start.
//!
//! **Token management surface.** `POST /v1/config/iam/tokens` mints a
//! workspace token (the cleartext is returned EXACTLY once, alongside the
//! secret-free view), `GET /v1/config/iam/tokens?workspace_id=…` lists
//! secret-free views, and `DELETE /v1/config/iam/tokens/{id}` revokes (the
//! engine and the persisted row, so a restart keeps it revoked). These routes
//! are mounted
//! INSIDE the guarded management surface, but they authorize differently from
//! the guard's default: the guard authenticates and then DELEGATES
//! authorization to the handler ([`RouteAuthz::TokenAdmin`]), which authorizes
//! `apikey.write`/`apikey.read` at the TARGET workspace — the workspace named
//! in the body/query for mint/list, the token's own workspace for revoke — so
//! the scope graph decides: a Global-bound admin passes for any workspace, a
//! workspace-bound admin only for its own. The delegation is fail-closed: the
//! handlers refuse (401) unless the guard stamped the authenticated principal
//! on the request, and the routes are only mounted when the guard is enabled.
//! Revoking your own current token is allowed — the request completes and
//! subsequent calls 401. Only preset role ids are accepted (unknown role →
//! 422). Route errors speak RFC-9457 problem+json like the rest of the
//! `/v1/config/*` family; authn/authz failures keep the guard's Managed
//! [`ErrorResponse`] envelope so every 401/403 on the plane has one shape.
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
//! **What P1 defers**: custom roles, org-level scopes, group rosters,
//! entitlements, and approval discharge.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_iam_contract::{
    ActionKey, ApiToken, ApiTokenId, AuthorizationDecision, AuthorizationRequest, PrincipalRef,
    ScopeRef, Timestamp, WorkspaceId,
};
use awaken_iam_core::{
    ApiTokenDirectory, ApiTokenMinter, Effect, EntropySource, Grant, GrantId, GrantSubject,
    IamError, IssuedApiToken, OsEntropy, PolicySet, RoleBinding, RoleId,
};
use awaken_iam_preset::named_role_catalog;
use awaken_protocol_managed::dto::ErrorResponse;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::delete;
use axum::{Json, Router};

/// Name of the bootstrap admin-token file under the management directory.
pub const ADMIN_TOKEN_FILE: &str = "admin-token";

/// Workspace the bootstrap admin token is bound to.
pub const BOOTSTRAP_WORKSPACE: &str = "wrkspc_default";

/// Service principal id of the bootstrap admin token.
pub const BOOTSTRAP_PRINCIPAL: &str = "mgmt-bootstrap";

/// Sentinel in `iam_role_binding.workspace` meaning the binding is at
/// [`ScopeRef::Global`], not at a workspace. `*` can never collide with a real
/// workspace id (they are plain `[A-Za-z0-9:_-]`). Used for the bootstrap
/// principal's global `admin` binding (see the module doc's bootstrap-scope
/// paragraph).
pub const GLOBAL_BINDING_WORKSPACE: &str = "*";

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
        self.mint_token(spec).map(|issued| issued.secret)
    }

    /// [`ManagementAuthz::mint_service_token`], keeping the persisted row too:
    /// the HTTP mint route returns the secret-free view alongside the one-time
    /// cleartext.
    fn mint_token(&self, spec: TokenSpec) -> Result<IssuedApiToken, String> {
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
        Ok(issued)
    }

    /// The live token row for `id`, if any (engine state, so a revocation done
    /// this process is visible immediately).
    fn token_by_id(&self, id: &str) -> Option<ApiToken> {
        self.state
            .lock()
            .unwrap()
            .directory
            .token(&ApiTokenId(id.to_string()))
            .cloned()
    }

    /// Revoke `id` in the live engine AND the persisted row (the row carries
    /// the full token serde, so rewriting it persists `revoked_at` across a
    /// restart). Idempotent like the engine; unknown id fails closed.
    fn revoke_token(&self, id: &str) -> Result<ApiToken, IamError> {
        let token_id = ApiTokenId(id.to_string());
        let mut guard = self.state.lock().unwrap();
        guard
            .directory
            .revoke(&token_id, Timestamp(now_rfc3339()))?;
        let token = guard
            .directory
            .token(&token_id)
            .expect("a just-revoked token is still stored")
            .clone();
        let data = serde_json::to_string(&token).expect("encode ApiToken row");
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE iam_api_token SET data = ?2 WHERE id = ?1",
                rusqlite::params![token.id.0, data],
            )
            .expect("persist token revocation");
        Ok(token)
    }

    /// Secret-free views of every persisted token bound to `workspace_id`,
    /// ordered by id. Rows and engine are written together under the state
    /// lock, so the rows are current.
    fn token_views(&self, workspace_id: &str) -> Vec<serde_json::Value> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT data, role FROM iam_api_token ORDER BY id")
            .expect("prepare token listing");
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .expect("query token rows");
        rows.filter_map(|row| {
            let (data, role) = row.expect("read token row");
            let token: ApiToken =
                serde_json::from_str(&data).expect("decode persisted ApiToken row");
            (token.workspace.0 == workspace_id).then(|| token_view(&token, role))
        })
        .collect()
    }

    /// The persisted mint-time role of token `id`, when recorded (rows written
    /// before the `role` column existed have none).
    fn token_role(&self, id: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT role FROM iam_api_token WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
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
/// `data` plus the mint-time `role` (for the secret-free view); keyed columns
/// exist only for uniqueness. A binding's `workspace` is either a workspace id
/// or the [`GLOBAL_BINDING_WORKSPACE`] sentinel for a Global-scope binding.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS iam_api_token (
    id     TEXT PRIMARY KEY,
    prefix TEXT NOT NULL UNIQUE,
    data   TEXT NOT NULL,
    role   TEXT
);
CREATE TABLE IF NOT EXISTS iam_role_binding (
    principal TEXT NOT NULL,
    role      TEXT NOT NULL,
    workspace TEXT NOT NULL,
    PRIMARY KEY (principal, role, workspace)
);
";

/// Idempotent column migration for installs whose `iam_api_token` predates the
/// `role` column: add it, then backfill each row's role from its principal's
/// binding at the token's workspace (mint wrote exactly that pair). A row with
/// no matching binding keeps NULL and its view simply omits `role`.
fn migrate_token_role_column(conn: &rusqlite::Connection) {
    let has_role = conn
        .prepare("SELECT role FROM iam_api_token LIMIT 0")
        .is_ok();
    if has_role {
        return;
    }
    conn.execute_batch("ALTER TABLE iam_api_token ADD COLUMN role TEXT")
        .expect("add iam_api_token.role column");
    let rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, data FROM iam_api_token")
            .expect("prepare role backfill scan");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query token rows for role backfill")
            .map(|row| row.expect("read token row"))
            .collect()
    };
    for (id, data) in rows {
        let token: ApiToken = serde_json::from_str(&data).expect("decode persisted ApiToken row");
        let principal = serde_json::to_string(&token.principal).expect("encode principal ref");
        let role: Option<String> = conn
            .query_row(
                "SELECT role FROM iam_role_binding WHERE principal = ?1 AND workspace = ?2",
                rusqlite::params![principal, token.workspace.0],
                |row| row.get(0),
            )
            .ok();
        if let Some(role) = role {
            conn.execute(
                "UPDATE iam_api_token SET role = ?2 WHERE id = ?1",
                rusqlite::params![id, role],
            )
            .expect("backfill token role");
        }
    }
}

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
    migrate_token_role_column(&conn);

    // Bootstrap scope fix (module doc): ensure the bootstrap principal's
    // `admin` role is bound at Global — without it the bootstrap credential
    // could only administer wrkspc_default and could not provision tokens for
    // any other workspace. INSERT OR IGNORE keeps this idempotent for fresh
    // bootstraps, restarts, and existing installs alike; the row is inserted
    // before hydration so the loop below binds it into the live policy. It is
    // inert unless a live token authenticates as the bootstrap principal, so
    // re-ensuring it after the bootstrap token is revoked grants nothing.
    let bootstrap_principal = serde_json::to_string(&PrincipalRef::Service {
        service_id: BOOTSTRAP_PRINCIPAL.to_string(),
    })
    .expect("encode bootstrap principal ref");
    conn.execute(
        "INSERT OR IGNORE INTO iam_role_binding (principal, role, workspace) VALUES (?1, ?2, ?3)",
        rusqlite::params![bootstrap_principal, "admin", GLOBAL_BINDING_WORKSPACE],
    )
    .expect("ensure the bootstrap principal's global admin binding");

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
            let scope = if workspace == GLOBAL_BINDING_WORKSPACE {
                ScopeRef::Global
            } else {
                ScopeRef::Workspace {
                    workspace_id: WorkspaceId(workspace),
                }
            };
            policy.bind_role(RoleBinding {
                principal: serde_json::from_str(&principal)
                    .expect("decode persisted principal ref"),
                role: RoleId(role),
                scope,
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
        "INSERT INTO iam_api_token (id, prefix, data, role) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![token.id.0, token.prefix.0, data, role],
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

/// How the guard authorizes a mapped management route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteAuthz {
    /// The default: fence any named `workspace_id` against the token's
    /// workspace, then authorize this action at the token's workspace scope.
    Scoped(&'static str),
    /// The `/v1/config/iam/tokens*` family: the guard authenticates and stamps
    /// the principal on the request; the handler authorizes `apikey.*` at the
    /// TARGET workspace (body/query for mint/list, the token's own workspace
    /// for revoke) so the scope graph — not header equality — decides
    /// cross-workspace reach. Fail-closed both ways: the handler 401s without
    /// the stamp, and these routes are only mounted when the guard is on.
    TokenAdmin,
}

/// The authenticated principal the guard stamps on a [`RouteAuthz::TokenAdmin`]
/// request for the handler's own authorization step.
#[derive(Debug, Clone)]
struct AuthedPrincipal(PrincipalRef);

/// The management-plane guard: authenticate the bearer token, fence any
/// `workspace_id` the request names against the token's workspace, map the
/// route to its management action, and authorize at the token's workspace
/// scope. 401 (`authentication_error`) for a missing/invalid/expired/revoked
/// token; 403 (`permission_error`) for a denied or approval-gated action or a
/// workspace mismatch — all in the Managed wire's [`ErrorResponse`] envelope so
/// SDK clients parse them. Token-management routes are the one exception
/// ([`RouteAuthz::TokenAdmin`]): authn here, target-workspace authz in the
/// handler.
pub async fn management_guard(
    State(authz): State<Arc<ManagementAuthz>>,
    req: Request,
    next: Next,
) -> Response {
    // Fail closed on an unmapped route: the guard only wraps the management
    // routers, so reaching this arm means a route was added without extending
    // the action table.
    let Some(route) = action_for(req.method(), req.uri().path()) else {
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

    let action = match route {
        RouteAuthz::Scoped(action) => action,
        RouteAuthz::TokenAdmin => {
            // Delegated authorization: no equality fence here — the handler
            // evaluates apikey.* at the TARGET workspace, and the scope graph
            // decides (Global binding ⇒ any workspace; workspace binding ⇒
            // that workspace only).
            let mut req = req;
            req.extensions_mut().insert(AuthedPrincipal(principal));
            return next.run(req).await;
        }
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

/// Map a management route + method to how it authorizes. Reads (GET) map to
/// `*.read`; every mutation maps to a write action. Credential, pool, and
/// vault surfaces live under `apikey.*`; catalog and admin aggregates under
/// `workspace.*` (see the constants above for the role-fit rationale). The
/// token-management family maps to [`RouteAuthz::TokenAdmin`] (target-workspace
/// authorization in the handler).
fn action_for(method: &Method, path: &str) -> Option<RouteAuthz> {
    let read = *method == Method::GET;
    let scoped = |action| Some(RouteAuthz::Scoped(action));
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        // -- catalog (providers / endpoints / offerings / snapshot) --
        ["v1", "config", "catalog"] if read => scoped(WORKSPACE_READ),
        ["v1", "config", "providers", _] | ["v1", "config", "endpoints", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "offerings"] if !read => scoped(WORKSPACE_WRITE),
        // -- credentials + pools --
        ["v1", "config", "credentials"] => scoped(if read { APIKEY_READ } else { APIKEY_WRITE }),
        ["v1", "config", "credentials", _] if read => scoped(APIKEY_READ),
        ["v1", "config", "credentials", _, "archive" | "validate"] if !read => scoped(APIKEY_WRITE),
        ["v1", "config", "credential-pools", _] => {
            scoped(if read { APIKEY_READ } else { APIKEY_WRITE })
        }
        // -- API-token management (authn here, target-workspace authz in the
        //    handler; see RouteAuthz::TokenAdmin) --
        ["v1", "config", "iam", "tokens"] | ["v1", "config", "iam", "tokens", _] => {
            Some(RouteAuthz::TokenAdmin)
        }
        // -- inference profiles + resolve dry-runs (secret-free projections) --
        ["v1", "config", "inference-profiles", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "inference-profiles", _, "resolve"] if !read => scoped(WORKSPACE_READ),
        ["v1", "config", "inference", "resolve"] if !read => scoped(WORKSPACE_READ),
        // -- MCP server defs + agent bindings --
        ["v1", "config", "mcp-servers"] if read => scoped(WORKSPACE_READ),
        ["v1", "config", "mcp-servers", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "agents", _, "mcp"] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "agents", _, "mcp", "resolve"] if !read => scoped(WORKSPACE_READ),
        // -- the Managed vault front door --
        ["v1", "vaults"] if !read => scoped(APIKEY_WRITE),
        ["v1", "vaults", _] => scoped(if read { APIKEY_READ } else { APIKEY_WRITE }),
        ["v1", "vaults", _, "credentials"] if !read => scoped(APIKEY_WRITE),
        ["v1", "vaults", _, "credentials", _] if read => scoped(APIKEY_READ),
        ["v1", "vaults", _, "credentials", _, "mcp_oauth_validate"] if !read => {
            scoped(APIKEY_WRITE)
        }
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

// ---- Token management routes ---------------------------------------------------

/// The HTTP token-management surface (`/v1/config/iam/tokens*`). Mounted
/// INSIDE the guarded management router — [`management_guard`] authenticates
/// first and stamps [`AuthedPrincipal`]; each handler then authorizes at its
/// TARGET workspace (see [`RouteAuthz::TokenAdmin`] and the module doc).
pub(crate) fn token_router(iam: Arc<ManagementAuthz>) -> Router {
    Router::new()
        .route(
            "/v1/config/iam/tokens",
            axum::routing::post(mint_token_route).get(list_tokens_route),
        )
        .route("/v1/config/iam/tokens/:id", delete(revoke_token_route))
        .with_state(iam)
}

/// Secret-free view of a managed API token: coordinates, workspace binding,
/// mint-time role, and lifecycle stamps — NEVER the argon2 hash or cleartext.
/// (`role` is absent only for rows persisted before the role column existed;
/// `expires_at`/`revoked_at` are absent when unset.)
fn token_view(token: &ApiToken, role: Option<String>) -> serde_json::Value {
    let principal_id = match &token.principal {
        PrincipalRef::Service { service_id } => service_id.clone(),
        PrincipalRef::Account { account_id } => account_id.0.clone(),
        PrincipalRef::ApiToken { token_id } => token_id.clone(),
    };
    let mut view = serde_json::json!({
        "id": token.id.0,
        "prefix": token.prefix.0,
        "principal_id": principal_id,
        "workspace_id": token.workspace.0,
        "created_at": token.created_at.0,
    });
    let map = view.as_object_mut().expect("view is an object");
    if let Some(role) = role {
        map.insert("role".into(), serde_json::Value::String(role));
    }
    if let Some(expires_at) = &token.expires_at {
        map.insert(
            "expires_at".into(),
            serde_json::Value::String(expires_at.0.clone()),
        );
    }
    if let Some(revoked_at) = &token.revoked_at {
        map.insert(
            "revoked_at".into(),
            serde_json::Value::String(revoked_at.0.clone()),
        );
    }
    view
}

/// RFC 9457 content type, as `awaken_api_contract::PROBLEM_JSON_CONTENT_TYPE`.
const PROBLEM_JSON_CONTENT_TYPE: &str = "application/problem+json";

/// Correlation-id header, as `awaken_api_contract::REQUEST_ID_HEADER`.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// An RFC-9457 `application/problem+json` response, byte-compatible with the
/// admin-config-api conventions for the `/v1/config/*` family — the same
/// members `awaken_api_contract::ApiError::new` renders (`type` as the
/// `urn:api-contract:problem:<code>` URN, `title`, `status`, `detail`, `code`,
/// `request_id`), built locally because the assembly's crate-boundary
/// allowlist does not admit the contract crate here. Authn/authz failures
/// instead keep the guard's Managed [`ErrorResponse`] envelope so every
/// 401/403 on the plane has one shape.
fn problem(status: StatusCode, code: &str, detail: String, headers: &HeaderMap) -> Response {
    let rid = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    (
        status,
        [(CONTENT_TYPE, PROBLEM_JSON_CONTENT_TYPE)],
        Json(serde_json::json!({
            "type": format!("urn:api-contract:problem:{}", code.replace('_', "-")),
            "title": "API token error",
            "status": status.as_u16(),
            "detail": detail,
            "code": code,
            "request_id": rid,
        })),
    )
        .into_response()
}

/// Whether `role` is one of the preset Anthropic role ids — the only roles the
/// mint surface accepts (custom roles are post-P1).
fn is_preset_role(role: &str) -> bool {
    named_role_catalog(&Timestamp(now_rfc3339()))
        .iter()
        .any(|def| def.id.0 == role)
}

/// The preset role ids, for the unknown-role error message.
fn preset_role_ids() -> String {
    named_role_catalog(&Timestamp(now_rfc3339()))
        .iter()
        .map(|def| def.id.0.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `value` has the contract's canonical RFC 3339 UTC shape
/// (`YYYY-MM-DDTHH:MM:SSZ`), so lexical comparison is chronological. The mint
/// engine additionally enforces `expires_at` strictly after `created_at`.
fn canonical_timestamp_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes[19] == b'Z'
        && [4usize, 7].iter().all(|&i| bytes[i] == b'-')
        && bytes[10] == b'T'
        && [13usize, 16].iter().all(|&i| bytes[i] == b':')
        && (0..19).all(|i| [4, 7, 10, 13, 16].contains(&i) || bytes[i].is_ascii_digit())
}

/// A fresh `tok_<hex>` row id from OS entropy (64 bits — collision-free at
/// single-machine token counts; a freak collision fails the row's PRIMARY KEY
/// and surfaces as a mint error rather than overwriting).
fn fresh_token_id() -> String {
    let mut bytes = [0u8; 8];
    OsEntropy.fill_bytes(&mut bytes);
    bytes.iter().fold("tok_".to_string(), |mut id, byte| {
        id.push_str(&format!("{byte:02x}"));
        id
    })
}

/// The guard's [`AuthedPrincipal`] stamp, or `None` when the route was somehow
/// reached without the guard — the caller 401s (the fail-closed direction).
fn stamped_principal(req_ext: Option<&AuthedPrincipal>) -> Option<PrincipalRef> {
    req_ext.map(|AuthedPrincipal(principal)| principal.clone())
}

/// The 401 for a token-management route reached without the guard's stamp.
fn missing_guard_stamp() -> Response {
    unauthorized("token management requires the embedded IAM guard")
}

/// Authorize `principal` for `action` at the TARGET workspace: `None` on
/// allow, `Some(refusal)` mapping a non-allow decision to the guard's 403
/// shapes.
fn authorize_at_target(
    authz: &ManagementAuthz,
    principal: PrincipalRef,
    action: &str,
    workspace_id: &str,
) -> Option<Response> {
    match authz.authorize(principal, action, WorkspaceId(workspace_id.to_string())) {
        AuthorizationDecision::Allow => None,
        AuthorizationDecision::RequireApproval => Some(forbidden(
            "this action requires approval, which the embedded P1 plane cannot grant",
        )),
        AuthorizationDecision::Deny => Some(forbidden(
            "the API token's role does not authorize token management for this workspace",
        )),
    }
}

/// `POST /v1/config/iam/tokens` — mint a workspace token. Requires
/// `apikey.write` at the workspace named in the body (the scope graph decides
/// cross-workspace reach). The cleartext credential is returned exactly once,
/// alongside the secret-free view; only the argon2id hash persists.
async fn mint_token_route(
    State(authz): State<Arc<ManagementAuthz>>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    let Some(principal) = stamped_principal(req.extensions().get::<AuthedPrincipal>()) else {
        return missing_guard_stamp();
    };
    let bytes = match axum::body::to_bytes(req.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "mint request body exceeds the management body limit".to_string(),
                &headers,
            );
        }
    };
    let invalid = |detail: String| {
        problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_token_spec",
            detail,
            &headers,
        )
    };
    let Ok(serde_json::Value::Object(body)) = serde_json::from_slice(&bytes) else {
        return invalid("mint body must be a JSON object".to_string());
    };
    let field = |name: &str| {
        body.get(name)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let Some(workspace_id) = field("workspace_id") else {
        return invalid("`workspace_id` (non-empty string) is required".to_string());
    };
    let Some(role) = field("role") else {
        return invalid("`role` (non-empty string) is required".to_string());
    };
    if !is_preset_role(&role) {
        return problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_role",
            format!(
                "unknown role `{role}`: the preset roles are {}",
                preset_role_ids()
            ),
            &headers,
        );
    }
    let expires_at = field("expires_at");
    if let Some(expires_at) = &expires_at
        && !canonical_timestamp_shape(expires_at)
    {
        return invalid(format!(
            "`expires_at` must be a canonical RFC 3339 UTC timestamp \
             (YYYY-MM-DDTHH:MM:SSZ), got `{expires_at}`"
        ));
    }

    // The TARGET-workspace authorization: a Global-bound admin passes for any
    // workspace, a workspace-bound admin only for its own.
    if let Some(refusal) = authorize_at_target(&authz, principal, APIKEY_WRITE, &workspace_id) {
        return refusal;
    }

    let token_id = fresh_token_id();
    // Default to a per-token service principal so tokens do not silently share
    // (and thereby aggregate) role bindings unless the operator asks for it.
    let service_id = field("principal_id").unwrap_or_else(|| format!("mgmt-{token_id}"));
    let issued = match authz.mint_token(TokenSpec {
        token_id,
        service_id,
        workspace_id,
        role: role.clone(),
        created_at: None,
        expires_at,
    }) {
        Ok(issued) => issued,
        Err(err) => return invalid(format!("mint refused: {err}")),
    };
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            // The one and only time the cleartext leaves the server.
            "token": issued.secret,
            "api_token": token_view(&issued.token, Some(role)),
        })),
    )
        .into_response()
}

/// `GET /v1/config/iam/tokens?workspace_id=…` — secret-free views of the
/// workspace's tokens. Requires `apikey.read` at the target workspace.
async fn list_tokens_route(
    State(authz): State<Arc<ManagementAuthz>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    let Some(principal) = stamped_principal(req.extensions().get::<AuthedPrincipal>()) else {
        return missing_guard_stamp();
    };
    let Some(workspace_id) = query.get("workspace_id").filter(|v| !v.is_empty()) else {
        return problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_token_query",
            "`workspace_id` query parameter is required".to_string(),
            &headers,
        );
    };
    if let Some(refusal) = authorize_at_target(&authz, principal, APIKEY_READ, workspace_id) {
        return refusal;
    }
    Json(authz.token_views(workspace_id)).into_response()
}

/// `DELETE /v1/config/iam/tokens/{id}` — revoke. Requires `apikey.write` at
/// the TOKEN'S workspace. Unknown id → 404. Revoking your own current token is
/// allowed: this request completes (the guard authenticated before the
/// revocation landed) and every subsequent call 401s.
async fn revoke_token_route(
    State(authz): State<Arc<ManagementAuthz>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    let Some(principal) = stamped_principal(req.extensions().get::<AuthedPrincipal>()) else {
        return missing_guard_stamp();
    };
    let Some(token) = authz.token_by_id(&id) else {
        return problem(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no API token with id `{id}`"),
            &headers,
        );
    };
    if let Some(refusal) = authorize_at_target(&authz, principal, APIKEY_WRITE, &token.workspace.0)
    {
        return refusal;
    }
    match authz.revoke_token(&id) {
        Ok(revoked) => Json(token_view(&revoked, authz.token_role(&id))).into_response(),
        Err(IamError::ApiTokenNotFound { .. }) => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no API token with id `{id}`"),
            &headers,
        ),
        Err(err) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "revoke_failed",
            format!("revocation failed: {err}"),
            &headers,
        ),
    }
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
        // Scoped(action) shorthand so the assertions below stay line-per-route.
        fn action_for(method: &Method, path: &str) -> Option<&'static str> {
            match super::action_for(method, path) {
                Some(RouteAuthz::Scoped(action)) => Some(action),
                Some(RouteAuthz::TokenAdmin) => panic!("{path} is not a Scoped route"),
                None => None,
            }
        }
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
    fn the_token_management_family_delegates_authorization_to_its_handlers() {
        for (method, path) in [
            (Method::POST, "/v1/config/iam/tokens"),
            (Method::GET, "/v1/config/iam/tokens"),
            (Method::DELETE, "/v1/config/iam/tokens/tok_x"),
        ] {
            assert_eq!(action_for(&method, path), Some(RouteAuthz::TokenAdmin));
        }
    }

    #[test]
    fn only_canonical_utc_timestamps_pass_the_expiry_shape_check() {
        assert!(canonical_timestamp_shape("2027-01-01T00:00:00Z"));
        assert!(!canonical_timestamp_shape("banana"));
        assert!(!canonical_timestamp_shape("2027-01-01T00:00:00+02:00"));
        assert!(!canonical_timestamp_shape("2027-01-01 00:00:00Z"));
        assert!(!canonical_timestamp_shape("2027-01-01T00:00:00.000Z"));
    }

    #[test]
    fn only_preset_role_ids_are_mintable() {
        for role in ["admin", "workspace_admin", "workspace_user"] {
            assert!(is_preset_role(role), "{role}");
        }
        assert!(!is_preset_role("superuser"));
        assert!(!is_preset_role(""));
    }

    #[test]
    fn fresh_token_ids_are_tok_prefixed_hex() {
        let id = fresh_token_id();
        assert!(id.starts_with("tok_"), "{id}");
        assert_eq!(id.len(), 4 + 16);
        assert!(id[4..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(fresh_token_id(), fresh_token_id());
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
