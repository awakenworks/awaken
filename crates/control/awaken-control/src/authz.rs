//! Embedded IAM for the management plane (ADR-0042/0043 P1): bearer `ApiToken`
//! authn + grant-based authz gating `/v1/config/*` and `/v1/vaults/*`.
//!
//! # Trust model
//!
//! Opt-in via `AWAKEN_MGMT_IAM=embedded` (requires `AWAKEN_MGMT_DIR`); the
//! default — the variable unset — is today's open single-machine behavior,
//! byte-identical. When enabled, every management route demands a bearer
//! credential in the Awaken `sk-awaken-<prefix>.<secret>` shape (`sk-ant-` is
//! accepted as a legacy alias during the deprecation window) — either
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
//! and then be revoked. The binding persists in the SqlStore as a real
//! `ScopeRef::Global` row and is ensured idempotently on every boot, so
//! existing installs bootstrapped before this fix gain it on their next start.
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
//! bindings — live in `awaken-iam-server`'s own `SqlStore` over
//! `<dir>/iam.sqlite` (its migration ledger, its `iam_*` tables). Every mint
//! writes BOTH the live engine and the store rows under one lock, so a restart
//! over the same directory authenticates previously minted tokens. Hydration
//! walks the store's bindings to their principals and reloads each principal's
//! tokens (the `ApiTokenRepo` port deliberately has no list-all). Installs from
//! the pre-SqlStore layout (hand-rolled singular `iam_api_token` /
//! `iam_role_binding` tables, kept while iam-server's rusqlite pin was
//! links-incompatible) are imported once at boot and the legacy tables renamed.
//!
//! **What P1 defers**: custom roles, org-level scopes, group rosters,
//! entitlements, and approval discharge.
//!
//! **iam-host (ADR-0048) — `IamGate` is the PDP; the guard is the Managed PEP.**
//! authn and authz run through iam-host's [`IamGate`] (rev `9aa91e1`), the single
//! policy-decision point. The gate is built via `IamGate::from_local_state` over
//! *this crate's own* [`LocalIamState`] — the argon2 token directory + the
//! default-deny policy this module seeds (preset role grants) and hydrates from
//! `<dir>/iam.sqlite`. So the durable rows, the minter, and the evaluator are the
//! shared iam engine, and the gate is the one authn/authz front door:
//! [`ManagementAuthz::authenticate`] calls `gate.authenticate_detailed` (which
//! surfaces the distinct expired / revoked / invalid reasons the guard maps to
//! its 401 messages), and [`ManagementAuthz::authorize`] calls `gate.authorize`.
//! The gate shares the single state lock, so mint's paired write (token row +
//! role binding) stays atomic to a concurrent authenticate/authorize.
//!
//! What stays in this crate is the Managed-specific **PEP** shell the guard needs
//! and the generic host `auth_layer` cannot express without re-supplying it all as
//! callbacks: `x-api-key` credentials (via [`bearer_token`]), the Managed
//! [`ErrorResponse`] 401/403 envelope, the workspace path/query/body fence, the
//! 413 body cap, and the `TokenAdmin` delegation. Reaching that lossless split is
//! what the host extensions at rev `9aa91e1` delivered — `from_local_state`
//! (single-lock embed), `authenticate_detailed`/`AuthReject` (the 401 reasons),
//! `authenticate_scoped` (token workspace), plus `scope_for` /
//! `extract_credential` / `render_auth_error` for a fresh consumer that adopts
//! `auth_layer` wholesale. The pre-existing black-box tests
//! (`tests/management_authz.rs`, `tests/management_tokens.rs`) are the equivalence
//! oracle and pass unchanged across this adoption.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_iam_contract::{
    ActionKey, ApiToken, ApiTokenId, AuthorizationDecision, AuthorizationRequest, PrincipalRef,
    ProjectId, ScopeRef, Timestamp, WorkspaceId,
};
use awaken_iam_core::{
    ApiTokenDirectory, ApiTokenMinter, Effect, EntropySource, Grant, GrantId, GrantSubject,
    IamError, IssuedApiToken, OsEntropy, RoleBinding, RoleId,
};
use awaken_iam_core::{ApiTokenRepo, RoleBindingRepo};
use awaken_iam_host::{AuthReject, HostConfig, IamClient, IamGate, LocalIamState, connect_remote};
use awaken_iam_preset::{named_role_catalog, seed_named_roles};
use awaken_iam_server::{AuthzApi, SqlStore, SqliteBackend, sqlite_migrated_store};
use awaken_protocol_managed::types::ErrorResponse;
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

/// Sentinel the LEGACY (pre-SqlStore) `iam_role_binding.workspace` column used
/// for a [`ScopeRef::Global`] binding. Only the one-time legacy import still
/// reads it; the SqlStore persists real `ScopeRef`s.
const LEGACY_GLOBAL_BINDING_WORKSPACE: &str = "*";

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

/// The embedded management-plane authorizer: authn (bearer token → principal)
/// and authz (principal × action × workspace scope → decision), with its
/// durable token/binding rows in `<dir>/iam.sqlite`.
///
/// authn/authz run through iam-host's [`IamGate`] (the single PDP), built over
/// the same [`LocalIamState`] this struct mints and hydrates into — the argon2
/// token directory and the default-deny policy (role grants + workspace-scoped
/// bindings), both in-memory and kept consistent with the SQLite rows.
pub struct ManagementAuthz {
    /// Authz engine + token directory behind one lock (host's [`LocalIamState`]),
    /// shared with `gate`. Also serializes mint (directory write + binding write)
    /// so a concurrent mint cannot interleave the two.
    state: Arc<Mutex<LocalIamState>>,
    /// iam-host gate over `state`: the authn/authz PDP the guard calls.
    gate: IamGate,
    /// iam-server's repository adapter over `<dir>/iam.sqlite` (tokens,
    /// bindings, role defs — its schema, its migration ledger).
    store: SqlStore<SqliteBackend>,
}

/// Remote awaken-iam relying-party adapter for a locally running management
/// plane. The cached user token authenticates local single-user requests and,
/// unless a separate service credential is configured, the remote PDP call.
pub struct RemoteManagementAuthz {
    gate: IamGate,
    user_token: String,
}

/// User-selectable identity posture for the local product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagementIdentityMode {
    /// Single-user local operation without login.
    NoLogin,
    /// Awaken Cloud account, reused by the local process.
    AwakenCloud,
    /// Separately configured embedded/self-managed IAM.
    SelfManaged,
}

impl ManagementIdentityMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "no-login" => Some(Self::NoLogin),
            "cloud" | "awaken-cloud" => Some(Self::AwakenCloud),
            "embedded" | "local" | "self-managed" => Some(Self::SelfManaged),
            _ => None,
        }
    }
}

impl RemoteManagementAuthz {
    /// Connect to the authoritative IAM service and fetch its JWKS. Startup is
    /// fail-closed when no cloud credential or trust anchor is available.
    pub fn connect(
        base_url: String,
        audience: String,
        issuer: String,
        user_token: String,
        service_token: Option<String>,
    ) -> Result<Arc<Self>, String> {
        let mut config = HostConfig::remote(base_url)
            .with_audience(audience)
            .with_issuer(issuer);
        config.service_token = Some(service_token.unwrap_or_else(|| user_token.clone()));
        let handle = connect_remote(&config).map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            gate: handle.gate,
            user_token,
        }))
    }

    fn authenticate(&self, presented: Option<String>) -> Result<PrincipalRef, AuthReject> {
        let token = presented.as_deref().unwrap_or(&self.user_token);
        self.gate
            .authenticate_detailed(token, &Timestamp(now_rfc3339()), now_unix())
            .map(|(principal, _)| principal)
    }

    fn authorize(
        &self,
        principal: PrincipalRef,
        action: &str,
        scope: ScopeRef,
    ) -> AuthorizationDecision {
        IamClient::authorize(
            &self.gate,
            AuthorizationRequest::direct(principal, ActionKey(action.to_string()), scope),
        )
    }
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
    /// cleartext `sk-awaken-…` credential.
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
        let LocalIamState { authz, directory } = &mut *guard;
        let issued = ApiTokenMinter::new(OsEntropy)
            .mint(directory, authz.policy_mut(), request)
            .map_err(|err| err.to_string())?;
        // Persist through the SqlStore ports, mirroring exactly what mint wrote
        // into the live engine: the token row and the principal→role binding at
        // the token's workspace scope.
        ApiTokenRepo::create(&self.store, issued.token.clone())
            .map_err(|err| format!("persist token row: {err}"))?;
        RoleBindingRepo::add(
            &self.store,
            RoleBinding {
                principal,
                role: RoleId(spec.role),
                scope: ScopeRef::Workspace {
                    workspace_id: WorkspaceId(spec.workspace_id),
                },
            },
        )
        .map_err(|err| format!("persist role binding: {err}"))?;
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
        // The row carries the full token (incl. `revoked_at`), so updating it
        // persists the revocation across a restart.
        ApiTokenRepo::update(&self.store, token.clone()).expect("persist token revocation");
        Ok(token)
    }

    /// Every persisted token, ordered by id. The `ApiTokenRepo` port has no
    /// list-all by design, so the walk goes bindings → principals → each
    /// principal's tokens (every mint writes a binding, so the walk is total).
    fn all_tokens(&self) -> Vec<ApiToken> {
        let mut principals: Vec<PrincipalRef> = Vec::new();
        for binding in RoleBindingRepo::list(&self.store).expect("list role bindings") {
            if !principals.contains(&binding.principal) {
                principals.push(binding.principal);
            }
        }
        let mut tokens: Vec<ApiToken> = principals
            .iter()
            .flat_map(|principal| {
                ApiTokenRepo::list_for_principal(&self.store, principal)
                    .expect("list principal tokens")
            })
            .collect();
        tokens.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        tokens.dedup_by(|a, b| a.id == b.id);
        tokens
    }

    /// Secret-free views of every persisted token bound to `workspace_id`,
    /// ordered by id. Rows and engine are written together under the state
    /// lock, so the rows are current.
    fn token_views(&self, workspace_id: &str) -> Vec<serde_json::Value> {
        self.all_tokens()
            .into_iter()
            .filter(|token| token.workspace.0 == workspace_id)
            .map(|token| {
                let role = self.role_of(&token);
                token_view(&token, role)
            })
            .collect()
    }

    /// The mint-time role of `token`, derived from its principal's persisted
    /// binding at the token's workspace (mint wrote exactly that pair; the
    /// bootstrap principal's extra Global binding carries the same role).
    fn role_of(&self, token: &ApiToken) -> Option<String> {
        let bindings = RoleBindingRepo::list_for_principal(&self.store, &token.principal)
            .expect("list principal bindings");
        bindings
            .iter()
            .find(|b| {
                matches!(&b.scope, ScopeRef::Workspace { workspace_id } if workspace_id == &token.workspace)
            })
            .or_else(|| bindings.first())
            .map(|b| b.role.0.clone())
    }

    /// The persisted mint-time role of token `id`, when derivable.
    fn token_role(&self, id: &str) -> Option<String> {
        let token = self.token_by_id(id)?;
        self.role_of(&token)
    }

    /// Authenticate a presented bearer credential through the gate, returning its
    /// principal and workspace binding — or the reason it failed, so the guard
    /// can answer the distinct expired / revoked / invalid 401s.
    fn authenticate(&self, presented: &str) -> Result<(PrincipalRef, WorkspaceId), AuthReject> {
        // now_unix is unused for API tokens (this plane mints no JWTs); pass 0.
        match self
            .gate
            .authenticate_detailed(presented, &Timestamp(now_rfc3339()), 0)
        {
            Ok((principal, Some(workspace))) => Ok((principal, workspace)),
            // A management credential is always a workspace-bound API token; a
            // resolved token with no workspace (only a JWT, never minted here) is
            // treated as unauthenticated.
            Ok((_, None)) => Err(AuthReject::Invalid),
            Err(reject) => Err(reject),
        }
    }

    /// Evaluate `principal` performing `action` at the token's workspace scope,
    /// through the gate (the same default-deny engine, one PDP).
    fn authorize(
        &self,
        principal: PrincipalRef,
        action: &str,
        scope: ScopeRef,
    ) -> AuthorizationDecision {
        let request = AuthorizationRequest::direct(principal, ActionKey(action.to_string()), scope);
        self.gate.authorize(request)
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
    embedded_iam_for_workspace(dir, BOOTSTRAP_WORKSPACE)
}

/// Open embedded IAM for the platform-provisioned local Workspace. Production
/// composition roots use this entry point so IAM bootstrap authority and
/// resource ownership share one durable coordinate rather than a compiled id.
pub fn embedded_iam_for_workspace(dir: &Path, workspace_id: &str) -> Arc<ManagementAuthz> {
    std::fs::create_dir_all(dir).expect("create AWAKEN_MGMT_DIR for embedded IAM");
    let db_path = dir.join("iam.sqlite");
    import_legacy_layout(&db_path);
    let backend =
        SqliteBackend::open_path(&db_path).expect("open iam.sqlite under AWAKEN_MGMT_DIR");
    let store = sqlite_migrated_store(backend, "iam").expect("migrate iam.sqlite");

    // Durable role catalog (idempotent upsert of the preset roles) — the PAP
    // and any external reader see the same catalog the evaluator derives from.
    let now = Timestamp(now_rfc3339());
    seed_named_roles(&store, &now).expect("seed the preset role catalog");

    // Bootstrap scope fix (module doc): ensure the bootstrap principal's
    // `admin` role is bound at Global — without it the bootstrap credential
    // could only administer wrkspc_default and could not provision tokens for
    // any other workspace. `RoleBindingRepo::add` is idempotent, and the row is
    // written before hydration so the loop below binds it into the live
    // policy. It is inert unless a live token authenticates as the bootstrap
    // principal, so re-ensuring it after the bootstrap token is revoked grants
    // nothing.
    RoleBindingRepo::add(
        &store,
        RoleBinding {
            principal: PrincipalRef::Service {
                service_id: BOOTSTRAP_PRINCIPAL.to_string(),
            },
            role: RoleId("admin".to_string()),
            scope: ScopeRef::Global,
        },
    )
    .expect("ensure the bootstrap principal's global admin binding");

    let mut directory = ApiTokenDirectory::new();
    let mut engine = AuthzApi::new();
    let policy = engine.policy_mut();

    // The preset Anthropic role catalog, as data: every role's action patterns
    // become `GrantSubject::Role` grants at Global scope. Global here is NOT a
    // wildcard of authority — a principal only *holds* a role where its
    // workspace-scoped RoleBinding covers, so the binding confines the reach.
    // Re-derived every boot (roles are seed data; custom roles are post-P1).
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
    // auto-hydrated): bindings into the policy, and — since `ApiTokenRepo` has
    // no list-all by design — each binding principal's tokens into the
    // directory. Every mint writes a binding, so the walk is total. Failing
    // closed on a corrupt row (panic) beats dropping a token silently.
    let mut hydrated_tokens = 0usize;
    let mut seen_principals: Vec<PrincipalRef> = Vec::new();
    for binding in RoleBindingRepo::list(&store).expect("list role bindings") {
        if !seen_principals.contains(&binding.principal) {
            seen_principals.push(binding.principal.clone());
            for token in ApiTokenRepo::list_for_principal(&store, &binding.principal)
                .expect("list principal tokens")
            {
                directory.create(token).expect("hydrate unique token row");
                hydrated_tokens += 1;
            }
        }
        engine.policy_mut().bind_role(binding);
    }

    // One lock over the authz engine + token directory, wrapped as the gate's
    // Local state; `ManagementAuthz` keeps a clone to mint/hydrate through the
    // same lock the gate reads under.
    let state = Arc::new(Mutex::new(LocalIamState {
        authz: engine,
        directory,
    }));
    let gate = IamGate::from_local_state(Arc::clone(&state));
    let authz = Arc::new(ManagementAuthz { state, gate, store });

    if hydrated_tokens == 0 {
        bootstrap_admin_token(&authz, dir, workspace_id);
    }
    authz
}

/// One-time import of the pre-SqlStore layout: the hand-rolled singular
/// `iam_api_token` / `iam_role_binding` tables (token serde in `data`, binding
/// workspace with the `*` Global sentinel). Rows are copied into staging so the
/// SqlStore boot path below re-persists them through its own ports, then the
/// legacy tables are renamed (`*_imported`) so the import never runs twice.
/// iam-server's own tables are plural (`iam_api_tokens`), so the two layouts
/// never collide in one file.
fn import_legacy_layout(db_path: &Path) {
    if !db_path.exists() {
        return;
    }
    let conn = rusqlite::Connection::open(db_path).expect("open iam.sqlite for legacy check");
    let has_legacy = conn
        .prepare("SELECT data FROM iam_api_token LIMIT 0")
        .is_ok();
    if !has_legacy {
        return;
    }
    // Read the legacy rows now; write them through the SqlStore after it has
    // migrated (same file, second connection is fine for SQLite).
    let tokens: Vec<ApiToken> = {
        let mut stmt = conn
            .prepare("SELECT data FROM iam_api_token ORDER BY id")
            .expect("prepare legacy token scan");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query legacy token rows");
        rows.map(|data| {
            serde_json::from_str(&data.expect("read legacy token row"))
                .expect("decode legacy ApiToken row")
        })
        .collect()
    };
    let bindings: Vec<RoleBinding> = {
        let mut stmt = conn
            .prepare("SELECT principal, role, workspace FROM iam_role_binding")
            .expect("prepare legacy binding scan");
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("query legacy binding rows");
        rows.map(|row| {
            let (principal, role, workspace) = row.expect("read legacy binding row");
            let scope = if workspace == LEGACY_GLOBAL_BINDING_WORKSPACE {
                ScopeRef::Global
            } else {
                ScopeRef::Workspace {
                    workspace_id: WorkspaceId(workspace),
                }
            };
            RoleBinding {
                principal: serde_json::from_str(&principal).expect("decode legacy principal ref"),
                role: RoleId(role),
                scope,
            }
        })
        .collect()
    };
    conn.execute_batch(
        "ALTER TABLE iam_api_token RENAME TO iam_api_token_imported;\n\
         ALTER TABLE iam_role_binding RENAME TO iam_role_binding_imported;",
    )
    .expect("retire legacy iam tables");
    drop(conn);

    // Re-open through the store and write the rows through its ports.
    let backend = SqliteBackend::open_path(db_path).expect("reopen iam.sqlite for legacy import");
    let store =
        sqlite_migrated_store(backend, "iam").expect("migrate iam.sqlite for legacy import");
    let mut imported = 0usize;
    for token in tokens {
        if ApiTokenRepo::get(&store, &token.id)
            .expect("probe imported token")
            .is_none()
        {
            ApiTokenRepo::create(&store, token).expect("import legacy token row");
            imported += 1;
        }
    }
    for binding in bindings {
        RoleBindingRepo::add(&store, binding).expect("import legacy binding row");
    }
    eprintln!(
        "awaken-server: embedded IAM imported {imported} legacy token row(s) into the iam-server store"
    );
}

/// First-boot bootstrap: mint the one `admin`-role token the operator starts
/// from, log its cleartext once to stderr, and write it to `<dir>/admin-token`
/// (mode 0600). Single-machine hand-off by design (P1); rotate it early.
fn bootstrap_admin_token(authz: &ManagementAuthz, dir: &Path, workspace_id: &str) {
    let secret = authz
        .mint_service_token(TokenSpec {
            token_id: "tok_mgmt_bootstrap".to_string(),
            service_id: BOOTSTRAP_PRINCIPAL.to_string(),
            workspace_id: workspace_id.to_string(),
            role: "admin".to_string(),
            created_at: None,
            expires_at: None,
        })
        .expect("mint the bootstrap admin token");
    let path = dir.join(ADMIN_TOKEN_FILE);
    write_owner_only(&path, &secret).expect("write the bootstrap admin-token file");
    eprintln!(
        "awaken-server: EMBEDDED IAM BOOTSTRAP — minted the admin API token \
         for principal `{BOOTSTRAP_PRINCIPAL}` in workspace `{workspace_id}`.\n\
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

// ---- Middleware --------------------------------------------------------------

/// How the guard authorizes a mapped management route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteAuthz {
    /// The default: fence any named `workspace_id` against the token's
    /// workspace, then authorize this action at the token's workspace scope.
    Scoped {
        action: &'static str,
        scope: ScopeClass,
    },
    /// The `/v1/config/iam/tokens*` family: the guard authenticates and stamps
    /// the principal on the request; the handler authorizes `apikey.*` at the
    /// TARGET workspace (body/query for mint/list, the token's own workspace
    /// for revoke) so the scope graph — not header equality — decides
    /// cross-workspace reach. Fail-closed both ways: the handler 401s without
    /// the stamp, and these routes are only mounted when the guard is on.
    TokenAdmin,
}

/// Resource classes whose target scope is centrally defined by the IAM resource
/// model. Route handlers never choose a scope ad hoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeClass {
    Workspace,
    Project,
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
        Err(AuthReject::Expired) => return unauthorized("API token is expired"),
        Err(AuthReject::Revoked) => return unauthorized("API token is revoked"),
        Err(AuthReject::Invalid) => return unauthorized("invalid API token"),
    };

    let (action, scope_class) = match route {
        RouteAuthz::Scoped { action, scope } => (action, scope),
        RouteAuthz::TokenAdmin => {
            // Delegated authorization: no equality fence here — the handler
            // evaluates apikey.* at the TARGET workspace, and the scope graph
            // decides (Global binding ⇒ any workspace; workspace binding ⇒
            // that workspace only).
            let mut req = req;
            req.extensions_mut().insert(AuthedPrincipal(principal));
            req.extensions_mut()
                .insert(awaken_tenancy::WorkspaceScope(workspace.0));
            return next.run(req).await;
        }
    };

    // Workspace path fence (ADR-0048 D3 / ADR-0051): a `/v1/workspaces/{ws}/…`
    // request is rewritten to its flat form by `workspace_path_scope`, which stamps
    // the path `{ws}` as `RequestTenancy`. It is a *selection*; a narrow management
    // token can only select its own workspace, so — like the query/body fence — it
    // must equal the token's workspace (else a token could author resources owned
    // by an arbitrary workspace via the path).
    if let Some(tenancy) = req
        .extensions()
        .get::<awaken_authz_enforce::RequestTenancy>()
        && tenancy.workspace_id != workspace.0
    {
        return forbidden("workspace path does not match the API token's workspace");
    }
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
    let mut req = Request::from_parts(parts, Body::from(bytes));
    // Publish the authenticated authority to inner ownership and durable-audit
    // layers. The IAM guard must be the outer layer so no unauthenticated caller
    // can select this scope and no audited write silently falls back to default.
    req.extensions_mut()
        .insert(awaken_tenancy::WorkspaceScope(workspace.0.clone()));

    let Some(target_scope) = target_scope(scope_class, &workspace.0, req.uri().path()) else {
        return forbidden("the route has no resolvable authorization target");
    };

    match authz.authorize(principal, action, target_scope) {
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

/// Management PEP for Awaken Cloud identity. The request's explicit bearer
/// overrides the cached single-user login. Scope comes only from trusted edge
/// resolution; a missing scope is denied instead of falling back to a compiled
/// workspace constant.
pub async fn cloud_management_guard(
    State(authz): State<Arc<RemoteManagementAuthz>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(route) = action_for(req.method(), req.uri().path()) else {
        return forbidden("no management action is mapped for this route");
    };
    let RouteAuthz::Scoped {
        action,
        scope: scope_class,
    } = route
    else {
        return forbidden("API-token administration belongs to self-managed IAM");
    };

    let principal = match authz.authenticate(bearer_token(req.headers())) {
        Ok(principal) => principal,
        Err(AuthReject::Expired) => return unauthorized("cloud access token is expired"),
        Err(AuthReject::Revoked) => return unauthorized("cloud access token is revoked"),
        Err(AuthReject::Invalid) => return unauthorized("invalid cloud access token"),
    };
    let workspace = req
        .extensions()
        .get::<awaken_authz_enforce::RequestTenancy>()
        .map(|scope| scope.workspace_id.clone())
        .or_else(|| {
            req.extensions()
                .get::<awaken_tenancy::WorkspaceScope>()
                .map(|scope| scope.0.clone())
        });
    let Some(workspace) = workspace else {
        return forbidden("no trusted workspace context was resolved");
    };

    let Some(target_scope) = target_scope(scope_class, &workspace, req.uri().path()) else {
        return forbidden("the route has no resolvable authorization target");
    };
    let authz_for_pdp = authz.clone();
    let principal_for_pdp = principal.clone();
    let decision = match tokio::task::spawn_blocking(move || {
        authz_for_pdp.authorize(principal_for_pdp, action, target_scope)
    })
    .await
    {
        Ok(decision) => decision,
        Err(_) => return forbidden("cloud IAM authorization transport failed"),
    };
    match decision {
        AuthorizationDecision::Allow => {
            req.extensions_mut().insert(principal);
            req.extensions_mut()
                .insert(awaken_tenancy::WorkspaceScope(workspace));
            next.run(req).await
        }
        AuthorizationDecision::RequireApproval => {
            forbidden("this action requires approval and was not executed")
        }
        AuthorizationDecision::Deny => forbidden("cloud IAM denied this action"),
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
    let scoped = |action| {
        Some(RouteAuthz::Scoped {
            action,
            scope: ScopeClass::Workspace,
        })
    };
    let project = |action| {
        Some(RouteAuthz::Scoped {
            action,
            scope: ScopeClass::Project,
        })
    };
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
        // -- the config authoring plane: rich AgentConfig drafts + lifecycle. The
        //    console authors here directly (distinct from the /v1/agents registry). --
        ["v1", "config", "agents"] if read => scoped(WORKSPACE_READ),
        ["v1", "config", "agents", _, "validate" | "publish"] if !read => scoped(WORKSPACE_WRITE),
        ["v1", "config", "agents", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        // -- projects (consumption-side addressing + per-project agent bindings) --
        ["v1", "config", "projects"] if read => scoped(WORKSPACE_READ),
        ["v1", "config", "projects", _] => project(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "config", "projects", _, "agents", _, "mcp"] => project(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        // -- the Managed vault front door --
        ["v1", "vaults"] => scoped(if read { APIKEY_READ } else { APIKEY_WRITE }),
        ["v1", "vaults", _] => scoped(if read { APIKEY_READ } else { APIKEY_WRITE }),
        ["v1", "vaults", _, "archive"] if !read => scoped(APIKEY_WRITE),
        ["v1", "vaults", _, "credentials"] => scoped(if read { APIKEY_READ } else { APIKEY_WRITE }),
        ["v1", "vaults", _, "credentials", _] => {
            scoped(if read { APIKEY_READ } else { APIKEY_WRITE })
        }
        [
            "v1",
            "vaults",
            _,
            "credentials",
            _,
            "mcp_oauth_validate" | "archive",
        ] if !read => scoped(APIKEY_WRITE),
        // -- user profiles (managed-account entities: workspace configuration) --
        ["v1", "user_profiles"] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "user_profiles", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "user_profiles", _, "enrollment_url"] if !read => scoped(WORKSPACE_WRITE),
        // -- the public agent registry (distinct from /v1/config/agents authoring) --
        ["v1", "agents"] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "agents", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "agents", _, "versions"] if read => scoped(WORKSPACE_READ),
        ["v1", "agents", _, "archive"] if !read => scoped(WORKSPACE_WRITE),
        // -- deployments + deployment runs --
        ["v1", "deployments"] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "deployments", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        [
            "v1",
            "deployments",
            _,
            "archive" | "pause" | "unpause" | "run",
        ] if !read => scoped(WORKSPACE_WRITE),
        ["v1", "deployment_runs"] | ["v1", "deployment_runs", _] if read => scoped(WORKSPACE_READ),
        // -- environments + the self-hosted work queue --
        ["v1", "environments"] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "environments", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        ["v1", "environments", _, "archive"] if !read => scoped(WORKSPACE_WRITE),
        // Poll + stats + list are reads; ack/heartbeat/stop + work update are writes.
        ["v1", "environments", _, "work"] if read => scoped(WORKSPACE_READ),
        ["v1", "environments", _, "work", "poll" | "stats"] if read => scoped(WORKSPACE_READ),
        ["v1", "environments", _, "work", _] => scoped(if read {
            WORKSPACE_READ
        } else {
            WORKSPACE_WRITE
        }),
        [
            "v1",
            "environments",
            _,
            "work",
            _,
            "ack" | "heartbeat" | "stop",
        ] if !read => scoped(WORKSPACE_WRITE),
        _ => None,
    }
}

/// Resolve the concrete IAM target from one centrally classified resource
/// family. Project ids are coordinates from the matched route, never trusted
/// body fields; the workspace was already established by the edge PEP.
fn target_scope(class: ScopeClass, workspace: &str, path: &str) -> Option<ScopeRef> {
    match class {
        ScopeClass::Workspace => Some(ScopeRef::Workspace {
            workspace_id: WorkspaceId(workspace.to_string()),
        }),
        ScopeClass::Project => {
            let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            let project_id = segments
                .windows(2)
                .find(|pair| pair[0] == "projects")
                .map(|pair| pair[1])
                .filter(|id| !id.is_empty())?;
            Some(ScopeRef::Project {
                workspace_id: WorkspaceId(workspace.to_string()),
                project_id: ProjectId(project_id.to_string()),
            })
        }
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
        .route("/v1/config/iam/tokens/{id}", delete(revoke_token_route))
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
    match authz.authorize(
        principal,
        action,
        ScopeRef::Workspace {
            workspace_id: WorkspaceId(workspace_id.to_string()),
        },
    ) {
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

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
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
#[path = "authz_tests.rs"]
mod tests;
