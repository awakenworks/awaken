// @formal surface obligation=tenancy.authority_never_widens
// @formal surface obligation=tenancy.selector_agreement
// @formal reason: This HTTP projection rejects malformed selectors, then delegates
// @formal reason: binding coverage and selector agreement to the one tenancy kernel.
// @formal reason: Unbound candidates remain subject to the caller's existing PDP.
//! One HTTP selector projection over the tenancy reconciliation kernel.

use awaken_agent_contract::ScopeId;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use awaken_tenancy::{Authority, ScopeClaim, resolve_scope, select_scope};
use axum::extract::Request;

use crate::RequestTenancy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceSelectionError {
    InvalidSelector,
    ConflictingSelector,
}

fn valid_selector(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c == ',')
}

/// A Key's verified Workspace supplies authority and the flat-route default.
/// Explicit selectors can only select within that authority. An unbound
/// authenticated principal supplies no such authority here: its selected or
/// installed Workspace must still pass the caller's existing PDP.
pub fn request_workspace_scope(
    request: &Request,
    credential_workspace: Option<&str>,
) -> Result<Option<String>, WorkspaceSelectionError> {
    let mut claims = Vec::new();
    if let Some(path) = request.extensions().get::<RequestTenancy>() {
        if !valid_selector(&path.workspace_id) {
            return Err(WorkspaceSelectionError::InvalidSelector);
        }
        claims.push(ScopeClaim::FromPath(path.workspace_id.clone()));
    }
    let mut headers = request.headers().get_all("anthropic-workspace-id").iter();
    if let Some(value) = headers.next() {
        let value = value
            .to_str()
            .map_err(|_| WorkspaceSelectionError::InvalidSelector)?;
        if headers.next().is_some() || !valid_selector(value) {
            return Err(WorkspaceSelectionError::InvalidSelector);
        }
        claims.push(ScopeClaim::FromHeader(value.to_owned()));
    }
    if let Some(bound) = credential_workspace {
        if !valid_selector(bound) {
            return Err(WorkspaceSelectionError::InvalidSelector);
        }
        return resolve_scope(&Authority::bound(ScopeId::from(bound)), &claims)
            .map(|scope| Some(scope.0))
            .map_err(|_| WorkspaceSelectionError::ConflictingSelector);
    }
    select_scope(&claims)
        .map(|selection| {
            selection.map(|scope| scope.0).or_else(|| {
                request
                    .extensions()
                    .get::<WorkspaceScope>()
                    .and_then(WorkspaceScope::non_empty)
                    .map(str::to_owned)
            })
        })
        .map_err(|_| WorkspaceSelectionError::ConflictingSelector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request as HttpRequest};

    #[test]
    fn every_workspace_selector_is_reconciled_against_the_key_before_effects() {
        // Cause/effect graph: C1 bound A/unbound principal; C2 installed B;
        // C3 explicit path absent/A/B; C4 Header absent/A/B. Effects: E1 exact
        // A for a bound flat Key despite B; E2 preserve a matching selector;
        // E3 reject every conflict before PDP/handler; E4 unbound selection is
        // only a candidate for the caller's PDP, never a grant. Decision rows:
        // K1 A+B+no selectors -> A; K2 path/Header A -> A; K3 either B -> reject;
        // K4 path/Header disagree -> reject; K5 unbound/no selector -> B;
        // K6 unbound agreed selector -> exact candidate; K7 no context -> None.
        for bound in [Some("A"), None] {
            for path in [None, Some("A"), Some("B")] {
                for header in [None, Some("A"), Some("B")] {
                    let mut request = HttpRequest::get("/v1/agents").body(Body::empty()).unwrap();
                    request
                        .extensions_mut()
                        .insert(WorkspaceScope("installed".into()));
                    if let Some(path) = path {
                        request.extensions_mut().insert(RequestTenancy {
                            workspace_id: path.into(),
                        });
                    }
                    if let Some(header) = header {
                        request
                            .headers_mut()
                            .insert("anthropic-workspace-id", HeaderValue::from_static(header));
                    }
                    let expected = if bound.is_some()
                        && [path, header].into_iter().flatten().any(|s| s != "A")
                        || path.is_some() && header.is_some() && path != header
                    {
                        Err(WorkspaceSelectionError::ConflictingSelector)
                    } else {
                        Ok(Some(
                            bound.or(path).or(header).unwrap_or("installed").to_owned(),
                        ))
                    };
                    assert_eq!(
                        request_workspace_scope(&request, bound),
                        expected,
                        "{bound:?}/{path:?}/{header:?}"
                    );
                }
            }
        }
        assert_eq!(
            request_workspace_scope(&HttpRequest::get("/").body(Body::empty()).unwrap(), None),
            Ok(None)
        );
    }

    #[test]
    fn malformed_or_repeated_selectors_never_become_scope_authority() {
        // M1 empty, whitespace or comma-joined Header -> invalid; M2 repeated
        // Header, even identical -> invalid; M3 empty explicit path/binding ->
        // invalid. Every malformed row rejects without a fallback to installed B.
        for value in ["", " ", "A B", "A,B"] {
            let request = HttpRequest::get("/")
                .header("anthropic-workspace-id", value)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                request_workspace_scope(&request, Some("A")),
                Err(WorkspaceSelectionError::InvalidSelector)
            );
        }
        let request = HttpRequest::get("/")
            .header("anthropic-workspace-id", "A")
            .header("anthropic-workspace-id", "A")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            request_workspace_scope(&request, Some("A")),
            Err(WorkspaceSelectionError::InvalidSelector)
        );
        let mut request = HttpRequest::get("/").body(Body::empty()).unwrap();
        request.extensions_mut().insert(RequestTenancy {
            workspace_id: String::new(),
        });
        assert_eq!(
            request_workspace_scope(&request, Some("A")),
            Err(WorkspaceSelectionError::InvalidSelector)
        );
        assert_eq!(
            request_workspace_scope(
                &HttpRequest::get("/").body(Body::empty()).unwrap(),
                Some("")
            ),
            Err(WorkspaceSelectionError::InvalidSelector)
        );
    }
}
