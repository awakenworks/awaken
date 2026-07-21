//! Tenancy as an edge aspect (ADR-0051).
//!
//! The neutral core is tenancy-agnostic: its **processing** logic never reads a
//! scope, and its **persistence** carries one opaque [`ScopeId`] the engine never
//! consults (see the `ScopedRepo` decorators in the adapter crates). This crate
//! ships only the edge-facing vocabulary:
//!
//! - [`ScopeId`] — the opaque tenant handle (today it denotes a workspace; the
//!   core never interprets it, so a future Org tier changes nothing here).
//! - [`ScopeClaim`] — a scope *claimed* by one ingress vehicle (token, path, or
//!   domain), before authorization.
//! - [`Authority`] — the set of scopes an authenticated principal may act in
//!   (a narrow token is a singleton; a broad org principal reaches a set, filled
//!   from the scope graph by the PDP adapter).
//! - [`resolve_scope`] — the pure reconciliation: *the token is authority, the
//!   URL/domain is selection*. A vehicle never widens authority; it only selects
//!   among the scopes the principal already holds.
//!
//! The authorization coordinate (`awaken_iam_contract::ScopeRef`) and the scope
//! graph live in `awaken-iam`; a [`ScopeId`] is translated to a `ScopeRef` only at
//! the PDP adapter (the ACL boundary), so this crate never depends on the authz
//! engine.

use serde::{Deserialize, Serialize};

/// Seeded Workspace used by the anonymous single-machine composition.
///
/// This is an ownership coordinate, not an authorization decision. Authenticated
/// compositions replace it at the edge with the Workspace selected by their PEP.
pub const DEFAULT_WORKSPACE_ID: &str = "default";

/// An opaque tenant/ownership handle. The core never interprets it — it does not
/// know whether the id denotes a workspace, an org, or any tier. Identity only.
///
/// It is deliberately a thin newtype: isolation is `WHERE scope_id = ?` at the
/// persistence boundary, and a change to the tenancy model leaves this type
/// untouched because the value is opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScopeId(pub String);

/// Opaque execution ownership coordinate carried by a durable dispatch.
///
/// This value is serializable and therefore is an identity claim, not proof of
/// authorization. An ingress edge must resolve it against an authenticated
/// [`Authority`] and pass the resulting [`VerifiedExecutionScope`] to trusted
/// composition code before constructing a dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionScopeRef(pub ScopeId);

/// A scope whose membership in an authenticated principal's authority has been
/// checked. It deliberately does not implement serialization: only the opaque
/// [`ExecutionScopeRef`] crosses a durable or network boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExecutionScope(ExecutionScopeRef);

impl VerifiedExecutionScope {
    /// Consume the verified value into its durable representation.
    #[must_use]
    pub fn into_ref(self) -> ExecutionScopeRef {
        self.0
    }
}

impl AsRef<ExecutionScopeRef> for VerifiedExecutionScope {
    fn as_ref(&self) -> &ExecutionScopeRef {
        &self.0
    }
}

/// The tenant workspace an ingress edge resolved for a session, handed to the core
/// so an edge projection (webhooks / usage) can stamp it. This is a **tenancy**
/// concept (orthogonal to the session-runtime contract, which never sees tenancy):
/// it lives here beside [`ScopeId`], the opaque handle it corresponds to, so the
/// managed session edge and its consumers depend on the tenancy contract rather
/// than on a protocol adapter. `None`/absent when the edge resolved no workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceScope(pub String);

/// The real Workspace into which a configuration publication is installed.
///
/// This request-local coordinate is supplied by trusted composition code. It is
/// deliberately distinct from [`WorkspaceScope`]: a reserved configuration
/// namespace may be the authoring scope while resources, credentials, and runtime
/// lookup still belong to this real Workspace. It is neither an authorization
/// grant nor a persisted resource field; the PEP must authorize the request before
/// installing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionWorkspace(pub String);

impl ScopeId {
    /// Borrow the underlying id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ScopeId {
    fn from(value: String) -> Self {
        ScopeId(value)
    }
}

impl From<&str> for ScopeId {
    fn from(value: &str) -> Self {
        ScopeId(value.to_string())
    }
}

impl std::fmt::Display for ScopeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A scope claimed by one ingress vehicle, before authorization.
///
/// The token claim is the **authority** anchor (it is established by
/// authentication, not trusted from the wire); the path and domain claims are
/// **selections** — an unauthenticated vehicle never grants scope, it only names
/// which of the principal's authorized scopes to act in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeClaim {
    /// The scope the authenticated principal is bound to (authority anchor).
    FromToken(ScopeId),
    /// A scope selected by the URL path (e.g. `/v1/workspaces/{ws}/…`).
    FromPath(String),
    /// A scope selected by the request host (e.g. `{ws}.host`, ADR-0048 D7).
    FromDomain(String),
}

impl ScopeClaim {
    /// The selection this claim expresses, if it is a selecting vehicle
    /// (path/domain). The token claim is authority, not selection, so it yields
    /// `None`.
    #[must_use]
    pub fn selection(&self) -> Option<ScopeId> {
        match self {
            ScopeClaim::FromPath(s) | ScopeClaim::FromDomain(s) => Some(ScopeId(s.clone())),
            ScopeClaim::FromToken(_) => None,
        }
    }
}

/// The set of scopes an authenticated principal may act in.
///
/// A narrow token is a singleton `[workspace]`; a broad (org-spanning) principal
/// reaches the org's workspaces, filled from the scope graph's `covers` by the
/// PDP adapter. Membership is the only test [`resolve_scope`] needs — for a
/// singleton, "member of `{s}`" and "equal to `s`" coincide, so the same rule
/// gives both the narrow-token fence and the broad-principal selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    reachable: Vec<ScopeId>,
}

impl Authority {
    /// A narrow authority: the principal may act in exactly this one scope.
    #[must_use]
    pub fn bound(scope: ScopeId) -> Self {
        Authority {
            reachable: vec![scope],
        }
    }

    /// A broad authority reaching a set of scopes (e.g. an org's workspaces).
    /// Empty input is rejected at resolution time (a principal with no reach can
    /// act nowhere).
    #[must_use]
    pub fn reaching(scopes: impl IntoIterator<Item = ScopeId>) -> Self {
        let mut reachable: Vec<ScopeId> = scopes.into_iter().collect();
        reachable.sort();
        reachable.dedup();
        Authority { reachable }
    }

    /// The scopes this principal can act in.
    #[must_use]
    pub fn reachable(&self) -> &[ScopeId] {
        &self.reachable
    }

    /// Whether the principal may act in `scope`.
    #[must_use]
    pub fn covers(&self, scope: &ScopeId) -> bool {
        self.reachable.iter().any(|s| s == scope)
    }

    /// Turn an untrusted opaque reference into a process-local verified scope.
    pub fn verify_execution_scope(
        &self,
        scope: &ExecutionScopeRef,
    ) -> Result<VerifiedExecutionScope, ScopeRejection> {
        if self.covers(&scope.0) {
            Ok(VerifiedExecutionScope(scope.clone()))
        } else {
            Err(ScopeRejection::NotAuthorized {
                selected: scope.0.clone(),
            })
        }
    }
}

/// Why a scope could not be resolved to a single authorized target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeRejection {
    /// A path/domain named a scope the principal is not authorized for (this is
    /// the narrow-token fence and the broad-principal `covers` check in one).
    NotAuthorized { selected: ScopeId },
    /// No vehicle selected a scope and the authority is not a singleton, so the
    /// target is ambiguous — the request must name a workspace.
    SelectionRequired,
    /// The principal reaches no scope at all (fail-closed).
    NoAuthority,
}

/// Data-independent reconciliation result used by [`resolve_scope`]. Keeping the
/// policy over indices makes the security-critical decision independent of the
/// representation used for scope identifiers, and gives formal verification a
/// finite production kernel to exhaustively explore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    Authority(usize),
    Selection(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KernelRejection {
    NotAuthorized(usize),
    SelectionRequired,
    NoAuthority,
}

fn resolve_indices<T: Eq>(
    authority: &[T],
    selections: &[T],
) -> Result<Resolution, KernelRejection> {
    if authority.is_empty() {
        return Err(KernelRejection::NoAuthority);
    }
    if selections.is_empty() {
        return if authority.len() == 1 {
            Ok(Resolution::Authority(0))
        } else {
            Err(KernelRejection::SelectionRequired)
        };
    }
    for (index, selected) in selections.iter().enumerate() {
        if !authority.iter().any(|scope| scope == selected) {
            return Err(KernelRejection::NotAuthorized(index));
        }
    }
    if selections[1..]
        .iter()
        .all(|selected| selected == &selections[0])
    {
        Ok(Resolution::Selection(0))
    } else {
        Err(KernelRejection::SelectionRequired)
    }
}

impl std::fmt::Display for ScopeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScopeRejection::NotAuthorized { selected } => {
                write!(f, "not authorized for scope `{selected}`")
            }
            ScopeRejection::SelectionRequired => {
                f.write_str("a workspace must be named for this request")
            }
            ScopeRejection::NoAuthority => f.write_str("the credential authorizes no scope"),
        }
    }
}

/// Reconcile the ingress claims against the principal's authority into a single
/// authorized [`ScopeId`] (ADR-0051 D4).
///
/// Rules, in one pass:
/// 1. **Selections** are the scopes named by path/domain vehicles (the token claim
///    is authority, never selection). *Every* selector is examined — not just the
///    first: a vehicle never widens authority, so a later, disagreeing selector
///    cannot be silently ignored.
/// 2. **Fail closed on coverage:** if *any* selector names a scope the authority
///    does not cover, the request is rejected [`ScopeRejection::NotAuthorized`],
///    even when an earlier selector was authorized. The narrow-token fence
///    (singleton authority ⇒ must equal) and the broad-principal check (⇒ must be
///    a member) are the same `covers` test.
/// 3. **Agreement:** the covered selectors must all name the *same* scope; a
///    disagreement is ambiguous, so the request must name one workspace
///    ([`ScopeRejection::SelectionRequired`]).
/// 4. If no selector is present, the target is the authority's sole scope when it
///    is a singleton, else [`ScopeRejection::SelectionRequired`].
///
/// Authentication (establishing the [`Authority`]) and the [`ScopeId`] →
/// `ScopeRef` translation happen outside this function; it is pure so the
/// reconciliation is exhaustively testable without an IAM engine.
pub fn resolve_scope(
    authority: &Authority,
    claims: &[ScopeClaim],
) -> Result<ScopeId, ScopeRejection> {
    // Collect EVERY selecting vehicle (path/domain); the token is authority, never a
    // selection. First-wins would let a disagreeing, unauthorized second selector
    // slip through unchecked (fail-open), so each selector is reconciled below.
    let selections: Vec<ScopeId> = claims.iter().filter_map(ScopeClaim::selection).collect();
    match resolve_indices(&authority.reachable, &selections) {
        Ok(Resolution::Authority(index)) => Ok(authority.reachable[index].clone()),
        Ok(Resolution::Selection(index)) => Ok(selections[index].clone()),
        Err(KernelRejection::NotAuthorized(index)) => Err(ScopeRejection::NotAuthorized {
            selected: selections[index].clone(),
        }),
        Err(KernelRejection::SelectionRequired) => Err(ScopeRejection::SelectionRequired),
        Err(KernelRejection::NoAuthority) => Err(ScopeRejection::NoAuthority),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(s: &str) -> ScopeId {
        ScopeId(s.to_string())
    }

    #[test]
    fn scope_id_is_an_opaque_newtype() {
        let a = ScopeId::from("wrkspc_acme");
        assert_eq!(a.as_str(), "wrkspc_acme");
        assert_eq!(a.to_string(), "wrkspc_acme");
        assert_eq!(a, ScopeId::from("wrkspc_acme".to_string()));
    }

    #[test]
    fn execution_scope_is_verified_against_authenticated_authority() {
        let authority = Authority::reaching([ws("ws_a"), ws("ws_b")]);
        let allowed = ExecutionScopeRef(ws("ws_b"));
        assert_eq!(
            authority
                .verify_execution_scope(&allowed)
                .expect("authority covers scope")
                .into_ref(),
            allowed
        );

        let denied = ExecutionScopeRef(ws("ws_z"));
        assert_eq!(
            authority.verify_execution_scope(&denied),
            Err(ScopeRejection::NotAuthorized {
                selected: ws("ws_z")
            })
        );
    }

    #[test]
    fn only_path_and_domain_are_selections() {
        assert_eq!(ScopeClaim::FromToken(ws("a")).selection(), None);
        assert_eq!(ScopeClaim::FromPath("a".into()).selection(), Some(ws("a")));
        assert_eq!(
            ScopeClaim::FromDomain("a".into()).selection(),
            Some(ws("a"))
        );
    }

    // --- narrow token (data plane): the token IS the scope --------------------

    #[test]
    fn bare_narrow_token_resolves_to_its_own_scope() {
        let auth = Authority::bound(ws("wrkspc_local"));
        let got = resolve_scope(&auth, &[ScopeClaim::FromToken(ws("wrkspc_local"))]);
        assert_eq!(got, Ok(ws("wrkspc_local")));
    }

    #[test]
    fn narrow_token_selecting_its_own_scope_passes_the_fence() {
        let auth = Authority::bound(ws("wrkspc_local"));
        let got = resolve_scope(
            &auth,
            &[
                ScopeClaim::FromToken(ws("wrkspc_local")),
                ScopeClaim::FromPath("wrkspc_local".into()),
            ],
        );
        assert_eq!(got, Ok(ws("wrkspc_local")));
    }

    #[test]
    fn narrow_token_cannot_widen_via_path() {
        let auth = Authority::bound(ws("wrkspc_local"));
        let got = resolve_scope(
            &auth,
            &[
                ScopeClaim::FromToken(ws("wrkspc_local")),
                ScopeClaim::FromPath("wrkspc_other".into()),
            ],
        );
        assert_eq!(
            got,
            Err(ScopeRejection::NotAuthorized {
                selected: ws("wrkspc_other")
            }),
            "a subdomain/path never widens a narrow token"
        );
    }

    #[test]
    fn narrow_token_cannot_widen_via_domain() {
        let auth = Authority::bound(ws("wrkspc_local"));
        let got = resolve_scope(&auth, &[ScopeClaim::FromDomain("wrkspc_evil".into())]);
        assert_eq!(
            got,
            Err(ScopeRejection::NotAuthorized {
                selected: ws("wrkspc_evil")
            })
        );
    }

    // --- broad principal (management/org): path selects within authority ------

    #[test]
    fn broad_principal_selects_a_member_workspace() {
        let auth = Authority::reaching([ws("ws_a"), ws("ws_b"), ws("ws_c")]);
        let got = resolve_scope(&auth, &[ScopeClaim::FromPath("ws_b".into())]);
        assert_eq!(got, Ok(ws("ws_b")));
    }

    #[test]
    fn broad_principal_is_denied_a_non_member_workspace() {
        let auth = Authority::reaching([ws("ws_a"), ws("ws_b")]);
        let got = resolve_scope(&auth, &[ScopeClaim::FromPath("ws_z".into())]);
        assert_eq!(
            got,
            Err(ScopeRejection::NotAuthorized {
                selected: ws("ws_z")
            })
        );
    }

    #[test]
    fn broad_principal_without_a_selection_is_ambiguous() {
        let auth = Authority::reaching([ws("ws_a"), ws("ws_b")]);
        let got = resolve_scope(&auth, &[ScopeClaim::FromToken(ws("ws_a"))]);
        assert_eq!(got, Err(ScopeRejection::SelectionRequired));
    }

    #[test]
    fn a_singleton_broad_authority_needs_no_selection() {
        // reaching a single scope collapses to the narrow case.
        let auth = Authority::reaching([ws("ws_solo")]);
        let got = resolve_scope(&auth, &[]);
        assert_eq!(got, Ok(ws("ws_solo")));
    }

    // --- fail-closed edges ----------------------------------------------------

    #[test]
    fn no_authority_is_denied() {
        let auth = Authority::reaching([]);
        assert_eq!(
            resolve_scope(&auth, &[ScopeClaim::FromPath("ws".into())]),
            Err(ScopeRejection::NoAuthority)
        );
    }

    #[test]
    fn two_both_authorized_disagreeing_selectors_are_ambiguous() {
        // path and domain are BOTH covered by the authority but name DIFFERENT
        // scopes. First-wins would silently take the path; instead the disagreement
        // is ambiguous and the request must name a single workspace.
        let auth = Authority::reaching([ws("ws_a"), ws("ws_b")]);
        let got = resolve_scope(
            &auth,
            &[
                ScopeClaim::FromPath("ws_a".into()),
                ScopeClaim::FromDomain("ws_b".into()),
            ],
        );
        assert_eq!(got, Err(ScopeRejection::SelectionRequired));
    }

    #[test]
    fn agreeing_path_and_domain_resolve_to_their_shared_scope() {
        // Both selectors name the same covered scope: they agree, so resolution
        // succeeds to that scope (the happy path for a dual-vehicle surface).
        let auth = Authority::reaching([ws("ws_a"), ws("ws_b")]);
        let got = resolve_scope(
            &auth,
            &[
                ScopeClaim::FromPath("ws_b".into()),
                ScopeClaim::FromDomain("ws_b".into()),
            ],
        );
        assert_eq!(got, Ok(ws("ws_b")));
    }

    #[test]
    fn reaching_dedups_and_sorts() {
        let auth = Authority::reaching([ws("b"), ws("a"), ws("b")]);
        assert_eq!(auth.reachable(), &[ws("a"), ws("b")]);
    }

    // --- ScopeRejection Display: the human-readable rejection copy -------------

    #[test]
    fn scope_rejection_display_messages() {
        assert_eq!(
            ScopeRejection::NotAuthorized {
                selected: ws("wrkspc_acme"),
            }
            .to_string(),
            "not authorized for scope `wrkspc_acme`"
        );
        assert_eq!(
            ScopeRejection::SelectionRequired.to_string(),
            "a workspace must be named for this request"
        );
        assert_eq!(
            ScopeRejection::NoAuthority.to_string(),
            "the credential authorizes no scope"
        );
    }

    // --- ScopeId is persisted as the `scope_id` column: pin the wire shape -----

    #[test]
    fn scope_id_serde_round_trips_as_a_bare_json_string() {
        let id = ScopeId::from("wrkspc_acme");
        // The newtype serializes transparently to the inner string — this is the
        // durable/wire contract for the `scope_id` column, so pin the exact shape.
        let json = serde_json::to_string(&id).expect("serialize ScopeId");
        assert_eq!(json, "\"wrkspc_acme\"");

        let back: ScopeId = serde_json::from_str(&json).expect("deserialize ScopeId");
        assert_eq!(back, id);
    }

    #[test]
    fn scope_id_deserializes_from_a_bare_json_string() {
        // A raw JSON string (as written by any producer of the column) reads back
        // into the newtype — the reverse direction of the wire contract.
        let back: ScopeId = serde_json::from_str("\"wrkspc_beta\"").expect("deserialize ScopeId");
        assert_eq!(back, ScopeId::from("wrkspc_beta"));
        assert_eq!(back.as_str(), "wrkspc_beta");
    }

    // --- WorkspaceScope newtype: construction + accessor + clone --------------

    #[test]
    fn workspace_scope_wraps_and_exposes_its_inner_id() {
        let scope = WorkspaceScope("wrkspc_acme".to_string());
        assert_eq!(scope.0, "wrkspc_acme");
        // Clone is the only other capability the newtype derives (Debug, Clone).
        let cloned = scope.clone();
        assert_eq!(cloned.0, "wrkspc_acme");
        // Debug is derived and includes the wrapped value.
        assert_eq!(format!("{scope:?}"), r#"WorkspaceScope("wrkspc_acme")"#);
    }

    // --- FAIL-CLOSED: every selector is checked; an unauthorized one is rejected --

    // A later, disagreeing selector that names an UNAUTHORIZED scope is rejected —
    // it is never masked by an earlier authorized selector (fail closed).
    #[test]
    fn a_disagreeing_unauthorized_second_selector_is_rejected() {
        // Authority reaches ONLY ws_a. The path selects ws_a (authorized); the
        // domain disagrees and selects ws_evil (NOT authorized). Every selector is
        // checked against the same authority, so the uncovered domain trips
        // NotAuthorized rather than being silently ignored.
        let auth = Authority::bound(ws("ws_a"));
        let got = resolve_scope(
            &auth,
            &[
                ScopeClaim::FromPath("ws_a".into()),
                ScopeClaim::FromDomain("ws_evil".into()),
            ],
        );
        assert_eq!(
            got,
            Err(ScopeRejection::NotAuthorized {
                selected: ws("ws_evil")
            }),
            "an unauthorized second selector is rejected, not masked by the first"
        );
    }

    // Symmetric case — domain first, path disagrees and is unauthorized: still
    // rejected. Ordering does not decide the outcome; coverage does.
    #[test]
    fn a_disagreeing_unauthorized_selector_is_rejected_regardless_of_order() {
        let auth = Authority::bound(ws("ws_a"));
        let got = resolve_scope(
            &auth,
            &[
                ScopeClaim::FromDomain("ws_a".into()),
                ScopeClaim::FromPath("ws_evil".into()),
            ],
        );
        assert_eq!(
            got,
            Err(ScopeRejection::NotAuthorized {
                selected: ws("ws_evil")
            })
        );
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn successful_scope_resolution_never_widens_authority() {
        let authority = [kani::any::<u8>(), kani::any::<u8>()];
        let selections = [kani::any::<u8>(), kani::any::<u8>()];

        if let Ok(Resolution::Selection(index)) = resolve_indices(&authority, &selections) {
            let resolved = selections[index];
            assert!(authority.contains(&resolved));
            assert!(selections.iter().all(|selected| *selected == resolved));
        }
    }

    #[kani::proof]
    fn any_uncovered_selector_fails_closed() {
        let authorized = kani::any::<u8>();
        let unauthorized = kani::any::<u8>();
        kani::assume(unauthorized != authorized);
        let authority = [authorized];
        let selections = [authorized, unauthorized];
        assert!(matches!(
            resolve_indices(&authority, &selections),
            Err(KernelRejection::NotAuthorized(1))
        ));
    }

    #[kani::proof]
    fn selector_order_cannot_change_an_authorized_result() {
        let a = kani::any::<u8>();
        let b = kani::any::<u8>();
        let authority = [a, b];
        let left = [a, b];
        let right = [b, a];
        assert_eq!(
            resolve_indices(&authority, &left).is_ok(),
            resolve_indices(&authority, &right).is_ok()
        );
    }
}
