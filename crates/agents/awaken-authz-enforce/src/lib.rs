//! Request enforcement for the agent runtime's HTTP surface — the open
//! "judgment" half of access control.
//!
//! It does four things and nothing else: (1) authenticate a presented bearer
//! credential against an in-memory [`ApiTokenDirectory`], (2) derive the
//! request's authorization scope from the URL (a `/projects/{id}` prefix anchors
//! at [`ScopeRef::Project`], a bare request at the token's home
//! [`ScopeRef::Workspace`]), (3) map the route to an [`ActionKey`], and (4)
//! authorize via the same default-deny [`PolicySet`] engine every Awaken product
//! shares. It is in-memory and seeded, so the single-machine standalone needs no
//! durable IAM store — that (minting HTTP surface, `awaken-iam-server`
//! persistence, multi-tenant provisioning) is the BuSL authoring half and lives
//! elsewhere.
//!
//! Scope fencing is free: a token whose `RoleBinding` sits at
//! `Workspace{ws_local}` cannot reach a `Project{ws_other, …}`, because the scope
//! graph resolves that project up to `Workspace{ws_other} → Global`, never to
//! `ws_local`. So an out-of-tenant request is denied even for an `admin` token.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_iam_contract::{
    ActionKey, AuthorizationDecision, AuthorizationRequest, PrincipalRef, ProjectId, ScopeRef,
    Timestamp, WorkspaceId,
};
use awaken_iam_core::{
    ApiTokenDirectory, ApiTokenMinter, Effect, Grant, GrantId, GrantSubject, IamError,
    MintApiToken, OsEntropy, PolicySet, RoleId,
};
use awaken_iam_preset::named_role_catalog;

/// The managed-agent / session surface's action namespace. The preset role
/// catalog grants `workspace.*` / `apikey.* / file.* / skill.*` but NOT this, so
/// [`EnforceEngine::seeded`] installs an `agent.*` grant for the `admin` role
/// (the model forbids a literal `*` pattern in a role, so it is granted here).
const AGENT_NAMESPACE: &str = "agent";

/// A request to mint an in-memory service token.
pub struct TokenSpec {
    /// Stable, unique token row id.
    pub token_id: String,
    /// Service principal the token authenticates as.
    pub service_id: String,
    /// Workspace the token's role binding sits at (its tenant fence).
    pub workspace_id: String,
    /// Preset role the principal holds at the workspace (`admin`, …).
    pub role: String,
    /// Optional RFC 3339 expiry (strictly after mint time).
    pub expires_at: Option<String>,
}

/// In-memory enforcement engine: a token directory + a policy, seeded with the
/// preset role catalog. Cheap to construct; the single-machine seeder mints a
/// couple of tokens into it at boot.
pub struct EnforceEngine {
    state: Mutex<State>,
}

struct State {
    directory: ApiTokenDirectory,
    policy: PolicySet,
}

impl EnforceEngine {
    /// A fresh engine with the preset Anthropic role catalog installed as
    /// Global `GrantSubject::Role` grants, plus an `agent.*` grant for `admin`
    /// so the managed-agent surface is reachable. A role is only *held* where a
    /// principal's `RoleBinding` covers, so Global grants are not wildcard
    /// authority — the binding confines the reach.
    #[must_use]
    pub fn seeded() -> Self {
        let now = Timestamp(now_rfc3339());
        let mut policy = PolicySet::new();
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
        // The managed-agent surface: grant `agent.*` to `admin` (the single
        // namespace the preset catalog omits, and which a role may not carry as
        // a bare `*`).
        policy.add_grant(Grant {
            id: GrantId("role:admin:agent".to_string()),
            subject: GrantSubject::Role(RoleId("admin".to_string())),
            action_pattern: awaken_iam_core::ActionPattern(format!("{AGENT_NAMESPACE}.*")),
            scope: ScopeRef::Global,
            effect: Effect::Allow,
        });
        Self {
            state: Mutex::new(State {
                directory: ApiTokenDirectory::new(),
                policy,
            }),
        }
    }

    /// Mint a service token bound to `role` at `workspace` (in-memory only).
    /// Returns the one-time cleartext `sk-ant-…` secret.
    pub fn mint(&self, spec: TokenSpec) -> Result<String, IamError> {
        let principal = PrincipalRef::Service {
            service_id: spec.service_id,
        };
        let request = MintApiToken {
            id: awaken_iam_contract::ApiTokenId(spec.token_id),
            principal,
            workspace: WorkspaceId(spec.workspace_id),
            role: RoleId(spec.role),
            created_at: Timestamp(now_rfc3339()),
            expires_at: spec.expires_at.map(Timestamp),
        };
        let mut guard = self.state.lock().expect("enforce engine poisoned");
        let state = &mut *guard;
        let issued = ApiTokenMinter::new(OsEntropy).mint(
            &mut state.directory,
            &mut state.policy,
            request,
        )?;
        Ok(issued.secret)
    }

    /// Authenticate a presented bearer credential → its principal and the
    /// workspace its role binding sits at. `Err` on unknown/expired/revoked.
    pub fn authenticate(&self, presented: &str) -> Result<(PrincipalRef, WorkspaceId), IamError> {
        let guard = self.state.lock().expect("enforce engine poisoned");
        let token = guard
            .directory
            .authenticate(presented, &Timestamp(now_rfc3339()))?;
        Ok((token.principal.clone(), token.workspace.clone()))
    }

    /// Authorize `principal` performing `action` at `scope`. Fail-closed: an
    /// unmatched request is [`AuthorizationDecision::Deny`].
    pub fn authorize(
        &self,
        principal: PrincipalRef,
        action: &ActionKey,
        scope: ScopeRef,
    ) -> AuthorizationDecision {
        let request = AuthorizationRequest::direct(principal, action.clone(), scope);
        self.state
            .lock()
            .expect("enforce engine poisoned")
            .policy
            .evaluate(&request)
            .decision
    }
}

/// Derive the authorization scope from a request's tenancy: a `/projects/{id}`
/// prefix anchors at [`ScopeRef::Project`] (fenced to `workspace_id`, which is
/// the *project's* workspace); a bare request anchors at the token's home
/// [`ScopeRef::Workspace`].
#[must_use]
pub fn request_scope(workspace_id: &str, project_id: Option<&str>) -> ScopeRef {
    match project_id {
        Some(project_id) => ScopeRef::Project {
            workspace_id: WorkspaceId(workspace_id.to_string()),
            project_id: ProjectId(project_id.to_string()),
        },
        None => ScopeRef::Workspace {
            workspace_id: WorkspaceId(workspace_id.to_string()),
        },
    }
}

/// Map a session/protocol route to its action: reads (`GET`/`HEAD`) →
/// `agent.read`, every mutation → `agent.write`. Both live under the `agent.*`
/// namespace granted to a runtime-capable role.
#[must_use]
pub fn session_action(method: &str) -> ActionKey {
    match method {
        "GET" | "HEAD" => ActionKey(format!("{AGENT_NAMESPACE}.read")),
        _ => ActionKey(format!("{AGENT_NAMESPACE}.write")),
    }
}

/// Days since the Unix epoch → RFC 3339 UTC, allocation-free of any date crate
/// (Howard Hinnant's public-domain `civil_from_days`).
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

    const WS: &str = "wrkspc_local";
    const PROJ: &str = "proj_local";

    fn engine_with_admin() -> (EnforceEngine, String, PrincipalRef) {
        let engine = EnforceEngine::seeded();
        let secret = engine
            .mint(TokenSpec {
                token_id: "tok_admin".into(),
                service_id: "operator".into(),
                workspace_id: WS.into(),
                role: "admin".into(),
                expires_at: None,
            })
            .expect("mint admin");
        let (principal, ws) = engine.authenticate(&secret).expect("authenticate");
        assert_eq!(ws.0, WS);
        (engine, secret, principal)
    }

    #[test]
    fn seeded_engine_mints_and_authenticates() {
        let (_engine, secret, principal) = engine_with_admin();
        assert!(
            secret.starts_with("sk-ant-"),
            "cleartext is Anthropic-shaped"
        );
        assert!(matches!(principal, PrincipalRef::Service { .. }));
    }

    #[test]
    fn an_unknown_token_is_rejected() {
        let engine = EnforceEngine::seeded();
        assert!(
            engine
                .authenticate("sk-ant-nope.definitely-not-a-token")
                .is_err()
        );
    }

    #[test]
    fn admin_is_authorized_at_its_own_project_and_workspace() {
        let (engine, _secret, principal) = engine_with_admin();
        // Project under the token's workspace: resolves up to Workspace{WS}.
        let at_project = engine.authorize(
            principal.clone(),
            &session_action("POST"),
            request_scope(WS, Some(PROJ)),
        );
        assert_eq!(at_project, AuthorizationDecision::Allow);
        // Bare (workspace-scoped) request.
        let at_workspace =
            engine.authorize(principal, &session_action("GET"), request_scope(WS, None));
        assert_eq!(at_workspace, AuthorizationDecision::Allow);
    }

    #[test]
    fn a_project_in_another_workspace_is_denied_even_for_admin() {
        let (engine, _secret, principal) = engine_with_admin();
        let cross = engine.authorize(
            principal,
            &session_action("POST"),
            request_scope("wrkspc_other", Some("proj_x")),
        );
        assert_eq!(cross, AuthorizationDecision::Deny, "the scope fence holds");
    }

    #[test]
    fn request_scope_distinguishes_project_from_bare() {
        assert!(matches!(
            request_scope(WS, Some(PROJ)),
            ScopeRef::Project { .. }
        ));
        assert!(matches!(
            request_scope(WS, None),
            ScopeRef::Workspace { .. }
        ));
    }

    #[test]
    fn session_action_maps_reads_and_writes() {
        assert_eq!(session_action("GET").0, "agent.read");
        assert_eq!(session_action("POST").0, "agent.write");
        assert_eq!(session_action("DELETE").0, "agent.write");
    }
}
