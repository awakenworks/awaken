//! Embedded IAM for the management plane (ADR-0042/0043 P1): bearer `ApiToken`
//! authn + grant-based authz gating `/v1/config/*` and `/v1/vaults/*`.
//!
//! # Trust model
//!
//! Typed deployment `identity_mode = "self-managed"` installs this guard;
//! `identity_mode = "open"` is the explicit local-machine mode. When enabled,
//! every management route demands a bearer
//! credential in the canonical Awaken `sk-awaken-<prefix>.<secret>` shape — either
//! `Authorization: Bearer …` or the SDK's `x-api-key` header). Secrets are
//! argon2id-hashed at rest by `awaken-iam-core`; the cleartext exists only in
//! the mint response and — for the bootstrap admin token — in
//! `<data_dir>/admin-token` (mode 0600).
//!
//! **Bootstrap contract.** On first boot over an empty token directory a
//! single `admin`-role token is minted for the service principal
//! `mgmt-bootstrap` in workspace `wrkspc_default` under the hidden local Org,
//! announced on stderr without its cleartext, and written to
//! `<dir>/admin-token` with a rotate-me warning. That file is the
//! single-machine operator hand-off; rotate by minting a successor admin token
//! through `POST /v1/config/iam/tokens` and revoking the bootstrap token
//! through `DELETE /v1/config/iam/tokens/{id}` (or from an embedding via
//! [`ManagementAuthz::mint_service_token`]).
//!
//! **Bootstrap scope.** The platform registers exactly `Org -> Workspace` in
//! the scope graph and binds the bootstrap principal at that Org. The Org is a
//! hidden default in single-machine mode (or an explicit platform coordinate),
//! while Workspace remains the finest Runtime authorization scope. Awaken has
//! no Project tenancy tier or project-scoped resource routes.
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
//! tokens (the `ApiTokenRepo` port deliberately has no list-all).
//!
//! **What P1 defers**: custom roles, group rosters, entitlements, and approval
//! discharge.
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

use awaken_authorization_contract::{
    RouteAccess, RouteGuardSelection, WorkspaceBindingRole, application_route_policy,
    legacy_workspace_binding_migration_target, run_backed_route_policy,
};
use awaken_iam_contract::{
    ActivateAuthorizationProfile, ApiToken, ApiTokenId, AuthorizationDecision,
    AuthorizationRequest, CreateAuthorizationProfile, OrgId, PolicySnapshot, PrincipalRef,
    ScopeRef, Timestamp, WorkspaceId,
};
#[cfg(test)]
use awaken_iam_contract::{GrantEffect, GrantSubjectRef, ScopeKind};
use awaken_iam_core::{
    ApiTokenDirectory, ApiTokenMinter, EntropySource, IamError, IssuedApiToken, OsEntropy,
    RoleBinding, RoleId,
};
use awaken_iam_core::{ApiTokenRepo, RoleBindingRepo};
#[cfg(test)]
use awaken_iam_core::{Effect, Grant, GrantId, GrantSubject};
use awaken_iam_host::{AuthReject, IamClient, IamGate, LocalIamState};
use awaken_iam_preset::{named_role_catalog, seed_named_roles};
use awaken_iam_server::{
    AuthorizationProfileAdmin, AuthzApi, SqlStore, SqliteBackend, sqlite_migrated_store,
};
use awaken_protocol_managed::types::ErrorResponse;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::delete;
use axum::{Json, Router};

mod bootstrap;
mod credentials;
mod local_browser;
mod profiles;
use credentials::{
    authorization_bearer_token, bearer_token, is_legacy_tunnel_route, is_tunnel_route,
    query_workspace_id,
};
mod remote;

use bootstrap::bootstrap_admin_token;
#[cfg(test)]
use profiles::{AUTHORIZATION_PROFILE_EPOCH, AWAKEN_WORKSPACE_CREDENTIAL_INGRESS_ROLE};
pub use profiles::{
    AWAKEN_WORKSPACE_HOSTED_ADMIN_ROLE, AWAKEN_WORKSPACE_HOSTED_BUILDER_ROLE,
    AWAKEN_WORKSPACE_POLICY_NAMESPACE, AWAKEN_WORKSPACE_PUBLISHER_ROLE,
    AWAKEN_WORKSPACE_TUNNEL_MANAGER_ROLE, AWAKEN_WORKSPACE_USER_ROLE,
    HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE, HOSTED_RUNTIME_POLICY_NAMESPACE,
    HOSTED_RUNTIME_WORKSPACE_ADMIN_ROLE, HOSTED_RUNTIME_WORKSPACE_USER_ROLE,
    hosted_runtime_authorization_profile, workspace_authorization_profile,
};
use profiles::{
    LEGACY_MANAGEMENT_POLICY_NAMESPACE, LEGACY_RESOURCE_POLICY_NAMESPACE, qualify_action,
    qualify_hosted_runtime_action, qualify_role,
};
pub use remote::RemoteManagementAuthz;

/// Name of the bootstrap admin-token file under the management directory.
pub const ADMIN_TOKEN_FILE: &str = "admin-token";

/// Workspace the bootstrap admin token is bound to.
pub const BOOTSTRAP_WORKSPACE: &str = "wrkspc_default";

/// Hidden Org used by a single-machine installation unless the platform
/// supplies `AWAKEN_ORG_ID`. It is a parent coordinate, not a UI concept.
pub const DEFAULT_ORG_ID: &str = "org_default";

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
const MODEL_SUPPLY_READ: &str = "model_supply.read";
const MODEL_SUPPLY_CONNECT: &str = "model_supply.connect";
const MODEL_SUPPLY_WRITE: &str = "model_supply.write";
const FILE_READ: &str = "file.read";
const FILE_WRITE: &str = "file.write";
const SKILL_READ: &str = "skill.read";
const SKILL_WRITE: &str = "skill.write";
const TUNNEL_MANAGE: &str = "tunnel.manage";
#[cfg(test)]
const RUN_CREATE: &str = "run.create";
#[cfg(test)]
const RUN_READ: &str = "run.read";

const CANONICAL_API_TOKEN_PREFIX: &str = "sk-awaken-";

fn persisted_role(role: &str) -> RoleId {
    if role.contains(':') {
        RoleId(role.to_owned())
    } else {
        qualify_role(role)
    }
}

fn local_role(role: &str) -> &str {
    role.strip_prefix(&format!("{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:"))
        .unwrap_or(role)
}

fn is_legacy_workspace_role(role: &str) -> bool {
    role.starts_with(&format!("{LEGACY_MANAGEMENT_POLICY_NAMESPACE}:"))
        || role.starts_with(&format!("{LEGACY_RESOURCE_POLICY_NAMESPACE}:"))
}

fn legacy_binding_coordinates(role: &str) -> Option<(bool, WorkspaceBindingRole)> {
    let (management, local) =
        if let Some(local) = role.strip_prefix(&format!("{LEGACY_MANAGEMENT_POLICY_NAMESPACE}:")) {
            (true, local)
        } else {
            (
                false,
                role.strip_prefix(&format!("{LEGACY_RESOURCE_POLICY_NAMESPACE}:"))?,
            )
        };
    let role = match local {
        "admin" => WorkspaceBindingRole::Admin,
        "developer" => WorkspaceBindingRole::Developer,
        "billing" => WorkspaceBindingRole::Billing,
        "user" => WorkspaceBindingRole::User,
        "claude_code_user" => WorkspaceBindingRole::ClaudeCodeUser,
        "workspace_admin" => WorkspaceBindingRole::WorkspaceAdmin,
        "workspace_developer" => WorkspaceBindingRole::WorkspaceDeveloper,
        "workspace_restricted_developer" => WorkspaceBindingRole::WorkspaceRestrictedDeveloper,
        "workspace_user" => WorkspaceBindingRole::WorkspaceUser,
        "workspace_billing" => WorkspaceBindingRole::WorkspaceBilling,
        "agent_publisher" => WorkspaceBindingRole::AgentPublisher,
        "credential_ingress" => WorkspaceBindingRole::CredentialIngress,
        "hosted_workspace_admin" => WorkspaceBindingRole::HostedWorkspaceAdmin,
        _ => return None,
    };
    Some((management, role))
}

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
    /// Hidden/platform Org that owns every workspace administered by this
    /// embedded single-machine IAM instance.
    org_id: OrgId,
    /// Installation workspace selected for browser sessions.
    workspace_id: WorkspaceId,
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
            role: qualify_role(&spec.role),
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
                role: qualify_role(&spec.role),
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
    /// Workspace-profile binding at the token's workspace.
    fn role_of(&self, token: &ApiToken) -> Option<String> {
        let bindings = RoleBindingRepo::list_for_principal(&self.store, &token.principal)
            .expect("list principal bindings");
        let workspace_bindings: Vec<_> = bindings
            .iter()
            .filter(|binding| {
                binding
                    .role
                    .0
                    .starts_with(AWAKEN_WORKSPACE_POLICY_NAMESPACE)
            })
            .collect();
        workspace_bindings
            .iter()
            .find(|b| {
                matches!(&b.scope, ScopeRef::Workspace { workspace_id } if workspace_id == &token.workspace)
            })
            .or_else(|| workspace_bindings.first())
            .map(|b| local_role(&b.role.0).to_owned())
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
        if !presented.starts_with(CANONICAL_API_TOKEN_PREFIX) {
            return Err(AuthReject::Invalid);
        }
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
        self.authorize_action(principal, action, scope, ActionNamespace::Workspace)
    }

    fn authorize_action(
        &self,
        principal: PrincipalRef,
        action: &str,
        scope: ScopeRef,
        namespace: ActionNamespace,
    ) -> AuthorizationDecision {
        let request =
            AuthorizationRequest::direct(principal, qualified_action(namespace, action), scope);
        self.gate.authorize(request)
    }

    /// Register a platform-owned workspace under this installation's hidden Org.
    /// This is PAP/PIP administration, not request authorization: composition
    /// roots call it only after the platform has created or resolved the workspace.
    /// Registering the hierarchy grants nothing by itself; the PDP still evaluates
    /// the caller's bindings and remains default-deny.
    pub fn register_workspace(&self, workspace_id: &str) {
        self.state
            .lock()
            .unwrap()
            .authz
            .policy_mut()
            .scope_graph_mut()
            .assign_workspace(WorkspaceId(workspace_id.to_owned()), self.org_id.clone());
    }
}

/// Open (or create) the embedded IAM state under `dir`: migrate
/// `<dir>/iam.sqlite`, install the preset role catalog as role grants, hydrate
/// persisted tokens + bindings into the live evaluator, and — when the token
/// directory is empty — mint the bootstrap admin token into `<dir>/admin-token`
/// and emit only a secret-free operator notice on stderr.
///
/// Panics on open/migrate failure, like the durable-store boot path: a
/// management server that silently came up open would be worse than one that
/// refuses to start.
pub fn embedded_iam(dir: &Path) -> Arc<ManagementAuthz> {
    embedded_iam_for_tenant(dir, DEFAULT_ORG_ID, BOOTSTRAP_WORKSPACE)
}

/// Open embedded IAM for the platform-provisioned local Workspace. Production
/// composition roots use this entry point so IAM bootstrap authority and
/// resource ownership share one durable coordinate rather than a compiled id.
pub fn embedded_iam_for_workspace(dir: &Path, workspace_id: &str) -> Arc<ManagementAuthz> {
    embedded_iam_for_tenant(dir, DEFAULT_ORG_ID, workspace_id)
}

/// Reconcile one product-owned immutable profile into the durable PAP and live
/// PDP. Equal documents hydrate the active revision; a changed built-in contract
/// appends and CAS-activates one revision instead of leaving restarts on a stale
/// policy or maintaining a consumer-side authorization bypass.
fn reconcile_builtin_profile(
    profiles: &AuthorizationProfileAdmin,
    engine: &mut AuthzApi,
    request: CreateAuthorizationProfile,
) {
    let namespace = request.namespace.clone();
    let active = profiles
        .active(&namespace)
        .expect("read active built-in authorization profile");
    if active
        .as_ref()
        .is_some_and(|profile| profile.document == request.document)
    {
        profiles
            .hydrate(engine, &PolicySnapshot::default(), &namespace)
            .expect("hydrate active built-in authorization profile");
        return;
    }

    let expected_active_revision = active.map(|profile| profile.revision);
    let draft = profiles
        .create_draft(request)
        .expect("create built-in authorization profile revision");
    let validation = profiles
        .validate(&namespace, draft.revision)
        .expect("validate built-in authorization profile revision");
    assert!(
        validation.valid,
        "invalid built-in authorization profile: {:?}",
        validation.errors
    );
    profiles
        .activate(
            engine,
            &PolicySnapshot::default(),
            &namespace,
            draft.revision,
            ActivateAuthorizationProfile {
                expected_active_revision,
            },
        )
        .expect("activate built-in authorization profile revision");
}

/// Remove one superseded active profile head after the canonical Workspace
/// profile is active. Immutable revisions remain in IAM as audit evidence.
fn retire_legacy_profile(
    profiles: &AuthorizationProfileAdmin,
    engine: &mut AuthzApi,
    namespace: &str,
) {
    let namespace = awaken_iam_contract::NamespaceId(namespace.to_owned());
    let Some(active) = profiles
        .active(&namespace)
        .expect("read legacy built-in authorization profile")
    else {
        return;
    };
    profiles
        .retire(
            engine,
            &PolicySnapshot::default(),
            &namespace,
            active.revision,
        )
        .expect("retire legacy built-in authorization profile");
}

/// Consolidate durable local bindings before live-PDP hydration. The old two
/// profile roles carried the same local role intent at the same scope, so both
/// map to one canonical Workspace role and are then removed. Validation is a
/// separate first pass: an incomplete authority pair cannot partially mutate
/// the store before the process refuses startup. A canonical binding left by a
/// prior interrupted migration authorizes removal of its remaining legacy rows.
fn migrate_legacy_workspace_bindings(store: &SqlStore<SqliteBackend>) -> Result<(), String> {
    let bindings = RoleBindingRepo::list(store).expect("list legacy role bindings");
    let mut migration = Vec::new();
    for legacy in &bindings {
        if !is_legacy_workspace_role(&legacy.role.0) {
            continue;
        }
        let Some((_, role)) = legacy_binding_coordinates(&legacy.role.0) else {
            return Err(format!(
                "unsupported legacy role `{}` for principal {:?} at {:?}",
                legacy.role.0, legacy.principal, legacy.scope
            ));
        };
        let has_management_binding = bindings.iter().any(|candidate| {
            candidate.principal == legacy.principal
                && candidate.scope == legacy.scope
                && legacy_binding_coordinates(&candidate.role.0) == Some((true, role))
        });
        let has_resource_binding = bindings.iter().any(|candidate| {
            candidate.principal == legacy.principal
                && candidate.scope == legacy.scope
                && legacy_binding_coordinates(&candidate.role.0) == Some((false, role))
        });
        let canonical = RoleBinding {
            principal: legacy.principal.clone(),
            role: qualify_role(role.canonical_local_name()),
            scope: legacy.scope.clone(),
        };
        let canonical_exists = bindings.contains(&canonical);
        if !canonical_exists
            && legacy_workspace_binding_migration_target(
                role,
                has_management_binding,
                has_resource_binding,
            )
            .is_none()
        {
            return Err(format!(
                "legacy role `{}` for principal {:?} at {:?} has no authority-equivalent canonical binding",
                legacy.role.0, legacy.principal, legacy.scope
            ));
        }
        migration.push((canonical, legacy.clone()));
    }
    for (canonical, legacy) in migration {
        RoleBindingRepo::add(store, canonical).expect("add canonical Workspace role binding");
        RoleBindingRepo::remove(store, &legacy).expect("remove legacy Workspace role binding");
    }
    if let Some(remaining) = RoleBindingRepo::list(store)
        .expect("list remaining legacy role bindings")
        .into_iter()
        .find(|binding| is_legacy_workspace_role(&binding.role.0))
    {
        return Err(format!(
            "legacy role `{}` remains for principal {:?} at {:?}",
            remaining.role.0, remaining.principal, remaining.scope
        ));
    }
    Ok(())
}

/// Open embedded IAM for the platform-owned `Org -> Workspace` coordinates.
/// Runtime deliberately has no Project authorization scope.
pub fn embedded_iam_for_tenant(
    dir: &Path,
    org_id: &str,
    workspace_id: &str,
) -> Arc<ManagementAuthz> {
    std::fs::create_dir_all(dir).expect("create typed data_dir for embedded IAM");
    let db_path = dir.join("iam.sqlite");
    let backend = SqliteBackend::open_path(&db_path).expect("open iam.sqlite under typed data_dir");
    let store = sqlite_migrated_store(backend, "iam").expect("migrate iam.sqlite");

    // Durable role catalog (idempotent upsert of the preset roles) — the PAP
    // and any external reader see the same catalog the evaluator derives from.
    let now = Timestamp(now_rfc3339());
    seed_named_roles(&store, &now).expect("seed the preset role catalog");
    migrate_legacy_workspace_bindings(&store).unwrap_or_else(|error| {
        panic!("legacy Workspace binding migration requires operator repair: {error}")
    });

    // Single-machine Runtime has one hidden Org and one platform-provisioned
    // Workspace. Bind bootstrap authority at the Org (never Global) so it can
    // reach registered child workspaces but cannot escape the product tenant.
    let legacy_global_bootstrap = RoleBinding {
        principal: PrincipalRef::Service {
            service_id: BOOTSTRAP_PRINCIPAL.to_string(),
        },
        role: qualify_role("admin"),
        scope: ScopeRef::Global,
    };
    if RoleBindingRepo::list(&store)
        .expect("list bindings before bootstrap scope migration")
        .contains(&legacy_global_bootstrap)
    {
        RoleBindingRepo::remove(&store, &legacy_global_bootstrap)
            .expect("remove legacy global bootstrap binding");
    }
    RoleBindingRepo::add(
        &store,
        RoleBinding {
            principal: PrincipalRef::Service {
                service_id: BOOTSTRAP_PRINCIPAL.to_string(),
            },
            role: qualify_role("admin"),
            scope: ScopeRef::Org {
                org_id: OrgId(org_id.to_owned()),
            },
        },
    )
    .expect("ensure the bootstrap principal's org admin binding");
    let mut directory = ApiTokenDirectory::new();
    let mut engine = AuthzApi::new();

    let profile_store = sqlite_migrated_store(
        SqliteBackend::open_path(&db_path).expect("reopen iam.sqlite for profile PAP"),
        "iam",
    )
    .expect("migrate authorization profile store");
    let profiles = AuthorizationProfileAdmin::new(Arc::new(profile_store));
    reconcile_builtin_profile(&profiles, &mut engine, workspace_authorization_profile());
    retire_legacy_profile(&profiles, &mut engine, LEGACY_MANAGEMENT_POLICY_NAMESPACE);
    retire_legacy_profile(&profiles, &mut engine, LEGACY_RESOURCE_POLICY_NAMESPACE);

    engine.policy_mut().scope_graph_mut().assign_workspace(
        WorkspaceId(workspace_id.to_owned()),
        OrgId(org_id.to_owned()),
    );

    // Hydrate the durable rows into the in-memory evaluator (it is never
    // auto-hydrated): bindings into the policy, and — since `ApiTokenRepo` has
    // no list-all by design — each binding principal's tokens into the
    // directory. Every mint writes a binding, so the walk is total. Failing
    // closed on a corrupt row (panic) beats dropping a token silently.
    let mut hydrated_tokens = 0usize;
    let mut seen_principals: Vec<PrincipalRef> = Vec::new();
    for binding in RoleBindingRepo::list(&store).expect("list role bindings") {
        if let ScopeRef::Workspace { workspace_id } = &binding.scope {
            engine
                .policy_mut()
                .scope_graph_mut()
                .assign_workspace(workspace_id.clone(), OrgId(org_id.to_owned()));
        }
        if !seen_principals.contains(&binding.principal) {
            seen_principals.push(binding.principal.clone());
            for token in ApiTokenRepo::list_for_principal(&store, &binding.principal)
                .expect("list principal tokens")
            {
                directory.create(token).expect("hydrate unique token row");
                hydrated_tokens += 1;
            }
        }
        engine.policy_mut().bind_role(RoleBinding {
            principal: binding.principal,
            role: persisted_role(&binding.role.0),
            scope: binding.scope,
        });
    }

    // One lock over the authz engine + token directory, wrapped as the gate's
    // Local state; `ManagementAuthz` keeps a clone to mint/hydrate through the
    // same lock the gate reads under.
    let state = Arc::new(Mutex::new(LocalIamState {
        authz: engine,
        directory,
    }));
    let gate = IamGate::from_local_state(Arc::clone(&state));
    let authz = Arc::new(ManagementAuthz {
        state,
        gate,
        store,
        org_id: OrgId(org_id.to_owned()),
        workspace_id: WorkspaceId(workspace_id.to_owned()),
    });

    if hydrated_tokens == 0 {
        bootstrap_admin_token(&authz, dir, workspace_id);
    }
    authz
}

// ---- Middleware --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionNamespace {
    Workspace,
    HostedRuntime,
}

fn qualified_action(namespace: ActionNamespace, action: &str) -> awaken_iam_contract::ActionKey {
    match namespace {
        ActionNamespace::Workspace => qualify_action(action),
        ActionNamespace::HostedRuntime => qualify_hosted_runtime_action(action),
    }
}

/// How the guard authorizes a mapped management route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteAuthz {
    /// Browser application protocol. The Coordinator mounts the canonical
    /// application-token guard on this family and process composition keeps it
    /// outside the service-token IAM edge. Reaching either management guard is
    /// therefore a fail-closed composition error.
    Application,
    /// The default: fence any named `workspace_id` against the token's
    /// workspace, then authorize this action at the token's workspace scope.
    Scoped {
        action: &'static str,
        scope: ScopeClass,
    },
    /// Resource-plane route. The canonical front-door PEP selects this policy
    /// namespace instead of stacking a second resource middleware.
    Resource {
        action: &'static str,
        scope: ScopeClass,
    },
    /// Hosted Session lifecycle. Cloud evaluates the Awaken-owned `run.*`
    /// profile; embedded/self-managed composition retains its existing
    /// resource-Workspace policy because it has no separately activated hosted
    /// release profile.
    HostedRuntime {
        action: &'static str,
        embedded_action: &'static str,
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

/// One typed policy declaration per bounded HTTP family. Concrete endpoint
/// membership stays in axum; this descriptor owns only the authorization
/// namespace and plane, avoiding a shadow copy of every route template. The
/// optional hosted route is also the release-owned distributed routing
/// contract: deployment tooling exports it instead of maintaining another
/// Coordinator path list.
#[derive(Debug, Clone, Copy)]
struct RoutePolicyDescriptor {
    prefix: &'static str,
    policy: RouteFamilyPolicy,
    hosted_runtime_route: Option<HostedRuntimeRouteDescriptor>,
}

/// Canonical flat route classification retained beside its IAM policy.
///
/// The public hosted profile derives both ingress spellings from this one
/// value. It deliberately does not store a second workspace-prefixed path.
#[derive(Debug, Clone, Copy)]
enum HostedRuntimeRouteDescriptor {
    PathPrefix(&'static str),
    PathTemplate(&'static str),
}

impl RoutePolicyDescriptor {
    const fn control(prefix: &'static str, policy: RouteFamilyPolicy) -> Self {
        Self {
            prefix,
            policy,
            hosted_runtime_route: None,
        }
    }

    const fn hosted_runtime(prefix: &'static str, policy: RouteFamilyPolicy) -> Self {
        Self {
            prefix,
            policy,
            hosted_runtime_route: Some(HostedRuntimeRouteDescriptor::PathPrefix(prefix)),
        }
    }

    const fn control_with_hosted_subtree(
        prefix: &'static str,
        policy: RouteFamilyPolicy,
        path_template: &'static str,
    ) -> Self {
        Self {
            prefix,
            policy,
            hosted_runtime_route: Some(HostedRuntimeRouteDescriptor::PathTemplate(path_template)),
        }
    }
}

impl HostedRuntimeRouteDescriptor {
    fn export(self) -> [HostedRuntimePathMatch; 2] {
        let flat = match self {
            Self::PathPrefix(path) => HostedRuntimePathMatch::PathPrefix {
                path: path.to_owned(),
            },
            Self::PathTemplate(path_template) => HostedRuntimePathMatch::PathTemplate {
                path_template: path_template.to_owned(),
            },
        };
        let canonical = match self {
            Self::PathPrefix(path) | Self::PathTemplate(path) => path,
        };
        let suffix = canonical
            .strip_prefix("/v1")
            .expect("hosted runtime route descriptors are canonical /v1 paths");
        let workspace = HostedRuntimePathMatch::PathTemplate {
            path_template: format!("/v1/workspaces/{{workspace_id}}{suffix}"),
        };
        [flat, workspace]
    }
}

/// One path matcher in the split-hosted Control-to-Coordinator facade.
///
/// `PathPrefix` maps directly to prefix-routing gateways. `PathTemplate` keeps
/// a shared family exact: `{name}` denotes one non-empty path segment, so a
/// deployment can compile it to its gateway's native matcher without routing
/// the Control-owned siblings beside it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "match", rename_all = "snake_case")]
pub enum HostedRuntimePathMatch {
    PathPrefix { path: String },
    PathTemplate { path_template: String },
}

/// Deterministic release contract consumed by hosted deployment routing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostedRuntimeRouteProfile {
    pub schema_version: u32,
    pub routes: Vec<HostedRuntimePathMatch>,
}

/// Project the Coordinator-owned browser surface from the same descriptors
/// that authorize it. This is deliberately a function rather than a public
/// mutable registry so the Awaken release remains the only route authority.
pub fn hosted_runtime_route_profile() -> HostedRuntimeRouteProfile {
    HostedRuntimeRouteProfile {
        schema_version: 1,
        routes: ROUTE_POLICIES
            .iter()
            .filter_map(|descriptor| descriptor.hosted_runtime_route)
            .flat_map(HostedRuntimeRouteDescriptor::export)
            .collect(),
    }
}

#[derive(Debug, Clone, Copy)]
enum RouteFamilyPolicy {
    Application,
    Scoped {
        read: &'static str,
        write: &'static str,
    },
    Resource {
        read: &'static str,
        write: &'static str,
    },
    RunBacked,
    TokenAdmin,
}

const HOSTED_RUN_POLICY: RouteFamilyPolicy = RouteFamilyPolicy::RunBacked;

const ROUTE_POLICIES: &[RoutePolicyDescriptor] = &[
    RoutePolicyDescriptor::hosted_runtime("/v1/sessions", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/dreams",
        RouteFamilyPolicy::Resource {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime("/v1/a2a", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime("/v1/message:send", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime("/v1/message:stream", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime("/v1/ai-sdk", RouteFamilyPolicy::Application),
    RoutePolicyDescriptor::hosted_runtime("/v1/ag-ui", RouteFamilyPolicy::Application),
    RoutePolicyDescriptor::hosted_runtime("/v1/durable", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/files",
        RouteFamilyPolicy::Resource {
            read: FILE_READ,
            write: FILE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/skills",
        RouteFamilyPolicy::Resource {
            read: SKILL_READ,
            write: SKILL_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/memory_stores",
        RouteFamilyPolicy::Resource {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/models",
        RouteFamilyPolicy::Resource {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime("/v1/awaken/sessions", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::control("/v1/config/iam/tokens", RouteFamilyPolicy::TokenAdmin),
    RoutePolicyDescriptor::hosted_runtime("/v1/application-access-tokens", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::control(
        "/v1/config/credentials",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/workspace-context",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/credential-pools",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/vaults",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/application-mcp-credentials",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/provider-connections",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_CONNECT,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/provider-descriptors",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/executable-models",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/model-attributes",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/catalog",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/capabilities",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/brokered-models",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/inference-profiles",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/inference",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/agents",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/agent-previews",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/publications",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/webhook-subscriptions",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/user_profiles",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    // MCP Tunnels are Cloud-owned resources exposed through the canonical
    // Management router.  Keep their public ACL in the same Workspace policy
    // namespace as the injected application port: reads may inspect only the
    // authenticated Workspace and every lifecycle operation is a write.  An
    // absent Cloud application still leaves the routes unmounted.
    RoutePolicyDescriptor::control(
        "/v1/tunnels",
        RouteFamilyPolicy::Scoped {
            read: TUNNEL_MANAGE,
            write: TUNNEL_MANAGE,
        },
    ),
    // Deprecated organization-shaped Tunnel wire. The credential remains
    // workspace-bound in Awaken, providing a deterministic migration mapping
    // without selecting an arbitrary Workspace from an Organization.
    RoutePolicyDescriptor::control(
        "/v1/organizations/tunnels",
        RouteFamilyPolicy::Scoped {
            read: TUNNEL_MANAGE,
            write: TUNNEL_MANAGE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/agents",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/deployments",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/deployment_runs",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    // Environment definitions are authored by Control, while only the
    // per-environment Work subtree is execution-owned by Coordinator.
    RoutePolicyDescriptor::control_with_hosted_subtree(
        "/v1/environments",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
        "/v1/environments/{environment_id}/work",
    ),
    RoutePolicyDescriptor::control(
        "/v1/awaken/sandbox-execution-policies",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/awaken/environments",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/awaken/memory-stores",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/capabilities",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
];

/// Resource classes whose target scope is centrally defined by the IAM resource
/// model. Route handlers never choose a scope ad hoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeClass {
    Workspace,
}

/// The authenticated principal the guard stamps on a [`RouteAuthz::TokenAdmin`]
/// request for the handler's own authorization step.
#[derive(Debug, Clone)]
pub(crate) struct AuthedPrincipal(pub(crate) PrincipalRef);

/// A2A v1 discovery is intentionally anonymous: peers need the standard card
/// before they can learn which authenticated runtime interface to call. Keep
/// this exception exact and read-only; every `/v1/a2a` operation still enters
/// the normal management/runtime authorization table.
fn is_public_protocol_discovery(method: &Method, path: &str) -> bool {
    method == Method::GET && path == "/.well-known/agent-card.json"
}

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
    if is_public_protocol_discovery(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    // Fail closed on an unmapped route: the guard only wraps the management
    // routers, so reaching this arm means a route was added without extending
    // the action table.
    let Some(route) = action_for(req.method(), req.uri().path()) else {
        return forbidden("no management action is mapped for this route");
    };
    // MCP Tunnels are a Cloud-only workload-identity surface. A self-managed
    // API token or browser session must never become a compatibility backdoor
    // for the public `/v1/tunnels` contract.
    if is_tunnel_route(req.uri().path()) {
        return unauthorized("Tunnel API requires a WIF bearer token");
    }

    let legacy_tunnel_route = is_legacy_tunnel_route(req.uri().path());
    let authenticated = if legacy_tunnel_route {
        bearer_token(req.headers())
            .map(|presented| authz.authenticate(&presented))
            .unwrap_or(Err(AuthReject::Invalid))
    } else {
        bearer_token(req.headers())
            .map(|presented| authz.authenticate(&presented))
            .unwrap_or_else(|| authz.authenticate_browser(req.headers()))
    };
    let (principal, workspace) = match authenticated {
        Ok(identity) => identity,
        Err(AuthReject::Expired) => return unauthorized("API token is expired"),
        Err(AuthReject::Revoked) => return unauthorized("API token is revoked"),
        Err(AuthReject::Invalid) => return unauthorized("invalid API token"),
    };

    let (action, scope_class, action_namespace) = match route {
        RouteAuthz::Application => {
            return forbidden("application protocol must use the application-token guard");
        }
        RouteAuthz::Scoped { action, scope } => (action, scope, ActionNamespace::Workspace),
        RouteAuthz::Resource { action, scope } => (action, scope, ActionNamespace::Workspace),
        RouteAuthz::HostedRuntime {
            embedded_action,
            scope,
            ..
        } => (embedded_action, scope, ActionNamespace::Workspace),
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

    let decision =
        authz.authorize_action(principal.clone(), action, target_scope, action_namespace);
    match decision {
        AuthorizationDecision::Allow => {
            req.extensions_mut().insert(AuthedPrincipal(principal));
            next.run(req).await
        }
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
    if is_public_protocol_discovery(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    let Some(route) = action_for(req.method(), req.uri().path()) else {
        return forbidden("no management action is mapped for this route");
    };
    let (action, scope_class, action_namespace) = match route {
        RouteAuthz::Application => {
            return forbidden("application protocol must use the application-token guard");
        }
        RouteAuthz::Scoped { action, scope } => (action, scope, ActionNamespace::Workspace),
        RouteAuthz::Resource { action, scope } => (action, scope, ActionNamespace::Workspace),
        RouteAuthz::HostedRuntime { action, scope, .. } => {
            (action, scope, ActionNamespace::HostedRuntime)
        }
        RouteAuthz::TokenAdmin => {
            return forbidden("API-token administration belongs to self-managed IAM");
        }
    };

    let tunnel_route = is_tunnel_route(req.uri().path());
    let legacy_tunnel_route = is_legacy_tunnel_route(req.uri().path());
    let presented = if tunnel_route {
        authorization_bearer_token(req.headers())
    } else {
        bearer_token(req.headers())
    };
    let authenticated = match authz.authenticate(presented) {
        Ok(authenticated) => authenticated,
        Err(AuthReject::Expired) => return unauthorized("cloud access token is expired"),
        Err(AuthReject::Revoked) => return unauthorized("cloud access token is revoked"),
        Err(AuthReject::Invalid) => return unauthorized("invalid cloud access token"),
    };
    if tunnel_route && !authenticated.is_managed_tunnel_workload() {
        return forbidden(
            "Tunnel API requires a WIF service token with workspace:manage_tunnels scope",
        );
    }
    if legacy_tunnel_route && authenticated.access_token_claims.is_some() {
        return forbidden("legacy Tunnel API requires an Admin API key");
    }
    let principal = authenticated.principal;
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
    let denial_detail = remote::cloud_authorization_denial_detail(
        &qualified_action(action_namespace, action),
        &workspace,
    );
    let authz_for_pdp = authz.clone();
    let principal_for_pdp = principal.clone();
    let decision = match tokio::task::spawn_blocking(move || {
        authz_for_pdp.authorize_action(principal_for_pdp, action, target_scope, action_namespace)
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
            forbidden(&format!("{denial_detail}; approval is required"))
        }
        AuthorizationDecision::Deny => forbidden(&denial_detail),
    }
}

/// Classify the bounded route family and derive its action by method. The axum
/// router remains the sole declaration of concrete routes: this function does
/// not repeat every endpoint or parameter shape. It only declares the stable
/// policy of each bounded family, so a newly mounted endpoint inherits that
/// family's read/write rule and an unknown family still fails closed.
fn action_for(method: &Method, path: &str) -> Option<RouteAuthz> {
    let is_read = matches!(*method, Method::GET | Method::HEAD);
    let access = if is_read {
        RouteAccess::Read
    } else {
        RouteAccess::Write
    };

    // The management guard is installed only over the composed management
    // Router. Its concrete route table is therefore the membership declaration;
    // these are the bounded management namespaces, not a second endpoint list.
    let descriptor = ROUTE_POLICIES
        .iter()
        .find(|descriptor| in_family(path, descriptor.prefix))?;

    // Resolution endpoints are side-effect-free previews even though their wire
    // method is POST. Derive the read action from the owning family instead of
    // maintaining a second hard-coded authorization vocabulary.
    if *method == Method::POST
        && (path == "/v1/config/inference/resolve"
            || path.ends_with("/resolve")
            || path.ends_with("/resolve-candidates"))
        && in_family(path, "/v1/config")
    {
        return match descriptor.policy {
            RouteFamilyPolicy::Application => {
                Some(route_from_contract(application_route_policy(access)))
            }
            RouteFamilyPolicy::Scoped { read, .. } => Some(RouteAuthz::Scoped {
                action: read,
                scope: ScopeClass::Workspace,
            }),
            RouteFamilyPolicy::Resource { read, .. } => Some(RouteAuthz::Resource {
                action: read,
                scope: ScopeClass::Workspace,
            }),
            RouteFamilyPolicy::RunBacked => Some(route_from_contract(run_backed_route_policy(
                RouteAccess::Read,
            ))),
            RouteFamilyPolicy::TokenAdmin => Some(RouteAuthz::TokenAdmin),
        };
    }
    Some(match descriptor.policy {
        RouteFamilyPolicy::Application => route_from_contract(application_route_policy(access)),
        RouteFamilyPolicy::Scoped { read, write } => RouteAuthz::Scoped {
            action: if is_read { read } else { write },
            scope: ScopeClass::Workspace,
        },
        RouteFamilyPolicy::Resource { read, write } => RouteAuthz::Resource {
            action: if is_read { read } else { write },
            scope: ScopeClass::Workspace,
        },
        RouteFamilyPolicy::RunBacked => route_from_contract(run_backed_route_policy(access)),
        RouteFamilyPolicy::TokenAdmin => RouteAuthz::TokenAdmin,
    })
}

fn route_from_contract(selection: RouteGuardSelection) -> RouteAuthz {
    match selection {
        RouteGuardSelection::Application => RouteAuthz::Application,
        RouteGuardSelection::HostedRuntime {
            action,
            embedded_action,
        } => RouteAuthz::HostedRuntime {
            action: action.as_str(),
            embedded_action: embedded_action.as_str(),
            scope: ScopeClass::Workspace,
        },
    }
}

fn in_family(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Resolve the concrete IAM target from one centrally classified resource
/// family. Runtime authorization intentionally stops at Workspace.
fn target_scope(class: ScopeClass, workspace: &str, _path: &str) -> Option<ScopeRef> {
    match class {
        ScopeClass::Workspace => Some(ScopeRef::Workspace {
            workspace_id: WorkspaceId(workspace.to_string()),
        }),
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
#[path = "authz/application_protocol_tests.rs"]
mod application_protocol_tests;
#[cfg(test)]
#[path = "authz/credential_authentication_tests.rs"]
mod credential_authentication_tests;
#[cfg(test)]
#[path = "authz_management_profile_tests.rs"]
mod management_profile_tests;
#[cfg(test)]
#[path = "authz/migration_tests.rs"]
mod migration_tests;
#[cfg(test)]
#[path = "authz/model_supply_tests.rs"]
mod model_supply_tests;
#[cfg(test)]
#[path = "authz_tests.rs"]
mod tests;
