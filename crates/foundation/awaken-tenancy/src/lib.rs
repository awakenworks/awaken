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

/// An opaque tenant/ownership handle. The core never interprets it — it does not
/// know whether the id denotes a workspace, an org, or any tier. Identity only.
///
/// It is deliberately a thin newtype: isolation is `WHERE scope_id = ?` at the
/// persistence boundary, and a change to the tenancy model leaves this type
/// untouched because the value is opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScopeId(pub String);

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
/// 1. **Selection** is the scope named by a path/domain vehicle (the first such
///    claim). The token claim is authority, never selection.
/// 2. If a selection is present, it must be covered by the authority — the
///    narrow-token fence (singleton authority ⇒ must equal) and the
///    broad-principal check (⇒ must be a member) are the same test. Otherwise
///    [`ScopeRejection::NotAuthorized`].
/// 3. If no selection is present, the target is the authority's sole scope when
///    it is a singleton, else [`ScopeRejection::SelectionRequired`].
///
/// Authentication (establishing the [`Authority`]) and the [`ScopeId`] →
/// `ScopeRef` translation happen outside this function; it is pure so the
/// reconciliation is exhaustively testable without an IAM engine.
pub fn resolve_scope(
    authority: &Authority,
    claims: &[ScopeClaim],
) -> Result<ScopeId, ScopeRejection> {
    if authority.reachable.is_empty() {
        return Err(ScopeRejection::NoAuthority);
    }
    // The first selecting vehicle wins; path and domain are equivalent selectors
    // (a deployment mounts at most one for a given surface, and if both are
    // present they must agree — both are checked against the same authority).
    let selection = claims.iter().find_map(ScopeClaim::selection);
    match selection {
        Some(selected) => {
            if authority.covers(&selected) {
                Ok(selected)
            } else {
                Err(ScopeRejection::NotAuthorized { selected })
            }
        }
        None => match authority.reachable.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(ScopeRejection::SelectionRequired),
        },
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
    fn the_first_selecting_vehicle_wins_and_is_still_authorized() {
        // path present and covered → resolves; a later domain claim doesn't override.
        let auth = Authority::reaching([ws("ws_a"), ws("ws_b")]);
        let got = resolve_scope(
            &auth,
            &[
                ScopeClaim::FromPath("ws_a".into()),
                ScopeClaim::FromDomain("ws_b".into()),
            ],
        );
        assert_eq!(got, Ok(ws("ws_a")));
    }

    #[test]
    fn reaching_dedups_and_sorts() {
        let auth = Authority::reaching([ws("b"), ws("a"), ws("b")]);
        assert_eq!(auth.reachable(), &[ws("a"), ws("b")]);
    }
}
