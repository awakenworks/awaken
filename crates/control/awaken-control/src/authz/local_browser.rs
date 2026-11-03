// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: This source implements the repository-controlled permission product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Local browser session composition over the embedded IAM identity store.

use std::sync::Arc;

use awaken_iam_contract::{AccountId, PrincipalRef, ScopeRef, Timestamp, WorkspaceId};
use awaken_iam_core::{RoleBinding, RoleBindingRepository};
use awaken_iam_host::{AuthReject, LocalBrowserAuth, LocalBrowserAuthError, LocalSetupHandoff};
use axum::http::HeaderMap;
use axum::http::header::COOKIE;

use super::{ManagementAuthz, qualify_role, rfc3339_at};

impl ManagementAuthz {
    /// Start the local browser handoff over this installation's existing,
    /// migrated IAM identity store and attach it to the canonical gate.
    pub fn begin_local_browser(
        &self,
        account_id: AccountId,
    ) -> Result<(LocalBrowserAuth, LocalSetupHandoff), LocalBrowserAuthError> {
        let (browser, handoff) = LocalBrowserAuth::begin_with_session_repository(
            account_id.clone(),
            Arc::new(self.store.clone()),
        )?;
        self.enable_local_browser(&browser, &account_id)?;
        Ok((browser, handoff))
    }

    /// Bind the stable local browser account as this installation's Org admin
    /// and attach IAM's canonical session authority to the existing gate.
    fn enable_local_browser(
        &self,
        browser: &LocalBrowserAuth,
        account_id: &AccountId,
    ) -> Result<(), LocalBrowserAuthError> {
        let principal = PrincipalRef::Account {
            account_id: account_id.clone(),
        };
        let binding = RoleBinding {
            principal: principal.clone(),
            role: qualify_role("admin"),
            scope: ScopeRef::Org {
                org_id: self.org_id.clone(),
            },
        };
        // State -> original attachment guard, both before persistence. SQL
        // acknowledgement is followed only by infallible in-memory publication.
        let mut state = self
            .iam_state()
            .map_err(|_| LocalBrowserAuthError::Unavailable)?;
        let attachment = browser.reserve_attachment(&self.gate)?;
        RoleBindingRepository::add(&self.store, binding.clone())
            .map_err(|_| LocalBrowserAuthError::Unavailable)?;
        state.authz.policy_mut().bind_role(binding);
        attachment.publish();
        Ok(())
    }

    pub(super) fn authenticate_browser(
        &self,
        headers: &HeaderMap,
    ) -> Result<(PrincipalRef, WorkspaceId), AuthReject> {
        let cookie = headers
            .get(COOKIE)
            .and_then(|value| value.to_str().ok())
            .ok_or(AuthReject::Invalid)?;
        self.gate
            .authenticate_session_cookie(cookie, Timestamp(rfc3339_at(self.clock.as_ref())))
            .map(|principal| (principal, self.workspace_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_iam_server::SqlConn as _;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::{IntoResponse, Response};
    use serde_json::json;
    use tower::ServiceExt as _;

    use crate::authz::{embedded_iam, management_guard};

    // Durable local-session cause/effect graph:
    // exchanged cookie -> hashed iam_sessions row; process reconstruction over
    // the same data_dir -> cookie principal enters the existing PDP; persisted
    // logout -> later reconstruction rejects it.
    //
    // Decision table:
    // | row | durable row | revoked | reconstructed process | effect |
    // | R1  | present     | no      | yes                   | protected route 200 |
    // | R2  | present     | yes     | yes                   | session route 401  |
    #[tokio::test]
    async fn session_survives_sqlite_restart_but_logout_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let account = AccountId("local-console-admin".into());
        let first_iam = embedded_iam(dir.path(), awaken_time::system_clock()).unwrap();
        let (first_browser, handoff) = first_iam.begin_local_browser(account.clone()).unwrap();
        let exchange = awaken_iam_host::local_browser_router(first_browser)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/local/exchange")
                    .header("host", "127.0.0.1:8080")
                    .header("origin", "http://127.0.0.1:8080")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "setup_token": handoff.setup_token }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = exchange.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        // R3 a binding write is unavailable after reserving the original
        // attachment -> refuse without live policy/version or binding effects;
        // the previous browser attachment survives and its cookie stays valid.
        let before = first_iam.state.lock().unwrap().authz.snapshot();
        first_iam
            .store
            .backend()
            .execute("PRAGMA query_only=ON", &[])
            .unwrap();
        let refused = AccountId("must-not-attach".into());
        assert!(matches!(
            first_iam.begin_local_browser(refused.clone()),
            Err(LocalBrowserAuthError::Unavailable)
        ));
        assert_eq!(first_iam.state.lock().unwrap().authz.snapshot(), before);
        assert!(
            RoleBindingRepository::list_for_principal(
                &first_iam.store,
                &PrincipalRef::Account {
                    account_id: refused
                }
            )
            .unwrap()
            .is_empty()
        );
        first_iam
            .store
            .backend()
            .execute("PRAGMA query_only=OFF", &[])
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, cookie.parse().unwrap());
        assert!(first_iam.authenticate_browser(&headers).is_ok());
        drop(first_iam);

        let restarted_iam = embedded_iam(dir.path(), awaken_time::system_clock()).unwrap();
        let (restarted_browser, _) = restarted_iam.begin_local_browser(account.clone()).unwrap();
        async fn echo() -> Response {
            (StatusCode::OK, "ok").into_response()
        }
        let protected = Router::new()
            .fallback(echo)
            .layer(axum::middleware::from_fn_with_state(
                restarted_iam,
                management_guard,
            ));
        let admitted = protected
            .oneshot(
                Request::builder()
                    .uri("/v1/config/catalog")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);

        let logout = awaken_iam_host::local_browser_router(restarted_browser)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/session")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::NO_CONTENT);

        let after_logout_iam = embedded_iam(dir.path(), awaken_time::system_clock()).unwrap();
        let (after_logout_browser, _) = after_logout_iam.begin_local_browser(account).unwrap();
        let rejected = awaken_iam_host::local_browser_router(after_logout_browser)
            .oneshot(
                Request::builder()
                    .uri("/v1/session")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    }
}
