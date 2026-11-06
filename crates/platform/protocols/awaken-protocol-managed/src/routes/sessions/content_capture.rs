// @formal surface requirement=analytics.observability.projection_only
// @formal reason: Read-only projection of the content-capture decision: the Workspace fence answers
// @formal reason: a foreign or unknown Session with one 404, and the decision is the shared kernel
// @formal reason: over the Host's policy, so this adapter owns no capture or authorization authority.
// @formal reason: Evidence: this module's equivalence and fence tables; a product-requirement
// @formal reason: classification, not a direct proof claim.
//! Awaken-only read projection of a Session's content-capture decision
//! (ADR-0050 D8), mounted by `crate::awaken_extensions::session_content_capture_router`.
//!
//! The answer is `{ requested, ceiling, consent, effective, reason }`: the
//! decision a Run of this Session makes now, computed by the Run boundary's own
//! kernel over the Host's `CapturePolicy`. A request attributed with
//! `anthropic-user-profile-id` (the header that attributes a turn) reads that
//! subject's consent; an unattributed one reports `consent: null`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;

use super::{WireErr, error_response, event_data_subject, session_not_found};
use crate::common::scope::RequiredWorkspaceScope;
use crate::state::ManagedState;

/// Read the decision for a Session the request's Workspace owns; any other id
/// answers the same 404 as an unknown one.
pub async fn get_session_content_capture(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
) -> Result<Json<awaken_data_governance_contract::CapturePolicyDecision>, WireErr> {
    let subject = event_data_subject(&headers)?.map(awaken_runtime_contract::DataSubjectId);
    match state.resolve_owner(&id).await.map_err(error_response)? {
        Some(owner) if owner == workspace => {}
        _ => return Err(session_not_found(&id)),
    }
    // A Run carries no requested level of its own (the Session request field
    // is not authored), so it asks for `Full`; the projection asks the same.
    let decision = state
        .capture_policy
        .decide(
            &workspace,
            awaken_runtime_contract::ContentCapture::Full,
            subject.as_ref(),
        )
        .await;
    Ok(Json(decision))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_data_governance_contract::{
        CapturePolicy, CapturePolicyInputs, CapturePolicySources, DataSubjectConsentSource,
        TelemetryNarrowing, TelemetryNarrowingSource, TelemetryPolicyError, decide_capture,
    };
    use awaken_runtime_contract::{ContentCapture, DataSubjectId, Purpose};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use crate::state::ManagedState;

    struct Narrowing(Result<TelemetryNarrowing, TelemetryPolicyError>);
    #[async_trait::async_trait]
    impl TelemetryNarrowingSource for Narrowing {
        async fn workspace_narrowing(
            &self,
            workspace: &str,
        ) -> Result<TelemetryNarrowing, TelemetryPolicyError> {
            assert_eq!(workspace, awaken_tenancy::DEFAULT_WORKSPACE_ID);
            self.0
        }
    }

    struct Consent(Option<ContentCapture>);
    #[async_trait::async_trait]
    impl DataSubjectConsentSource for Consent {
        async fn consent_ceiling(
            &self,
            subject: &DataSubjectId,
            purpose: Purpose,
        ) -> ContentCapture {
            assert_eq!(subject.as_str(), "dsub_reader");
            assert_eq!(purpose, Purpose::TelemetryContent);
            self.0.expect("an unattributed read never asks for consent")
        }
    }

    async fn session_state(policy: CapturePolicy) -> (Arc<ManagedState>, String) {
        let mut state =
            ManagedState::new(awaken_protocol_managed_testkit::CoordinatedRuntimeFake::default())
                .await;
        state.capture_policy = policy;
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent": "coder", "environment_id": "env_local"
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .unwrap();
        (Arc::new(state), session.id)
    }

    async fn read(
        state: &Arc<ManagedState>,
        workspace: &str,
        session: &str,
        subject: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let app = crate::awaken_extensions::session_content_capture_router(
            super::get_session_content_capture,
        )
        .with_state(state.clone())
        .layer(axum::Extension(
            awaken_runtime_contract::runs::scope::WorkspaceScope(workspace.to_owned()),
        ));
        let mut request = Request::get(format!("/v1/awaken/sessions/{session}/content-capture"));
        if let Some(subject) = subject {
            request = request.header("anthropic-user-profile-id", subject);
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    #[tokio::test]
    async fn session_content_capture_is_the_run_decision_for_every_cause() {
        // Equivalence with the Run boundary and the capture-decision read:
        // causes C1 Host ceiling (each level); C2 the Session Workspace's
        // narrowing answer (unset, each level, unreadable); C3 the
        // `anthropic-user-profile-id` attribution (absent, or a subject whose
        // consent is each level). Effects: E1 the body is exactly
        // `decide_capture(requested = Full, C1, C2, C3)`, the decision a Run
        // makes; E2 an unattributed read reports `consent: null` and never
        // reads consent.
        let levels = [
            ContentCapture::Off,
            ContentCapture::Structured,
            ContentCapture::Full,
        ];
        let mut answers = vec![
            Ok(TelemetryNarrowing::default()),
            Err(TelemetryPolicyError::StoreUnavailable),
            Err(TelemetryPolicyError::AuthorityUnreachable),
        ];
        answers.extend(levels.map(|level| {
            Ok(TelemetryNarrowing {
                content_capture: Some(level),
            })
        }));
        let consents = [None, Some(levels[0]), Some(levels[1]), Some(levels[2])];
        let mut rows = 0;
        for top in levels {
            for answer in answers.iter().copied() {
                for consent in consents {
                    let (state, session) = session_state(CapturePolicy::new(
                        top,
                        CapturePolicySources {
                            consent: Arc::new(Consent(consent)),
                            narrowing: Arc::new(Narrowing(answer)),
                        },
                    ))
                    .await;
                    let subject = consent.map(|_| "dsub_reader");
                    let (status, body) = read(
                        &state,
                        awaken_tenancy::DEFAULT_WORKSPACE_ID,
                        &session,
                        subject,
                    )
                    .await;
                    assert_eq!(status, StatusCode::OK);
                    let expected = decide_capture(CapturePolicyInputs {
                        requested: ContentCapture::Full,
                        top,
                        workspace: answer,
                        consent,
                    });
                    assert_eq!(
                        body,
                        serde_json::to_value(expected).unwrap(),
                        "E1 {top:?} {answer:?} {consent:?}"
                    );
                    if consent.is_none() {
                        assert!(body["consent"].is_null(), "E2");
                    }
                    rows += 1;
                }
            }
        }
        assert_eq!(rows, 3 * 6 * 4);
    }

    #[tokio::test]
    async fn session_content_capture_is_fenced_to_the_owning_workspace() {
        // Causes: the addressed id is owned by the request Workspace, owned by
        // another Workspace, or unknown; or the attribution header is blank.
        // Effects: owned -> 200; another Workspace or unknown -> the same 404
        // (existence is never disclosed); blank attribution -> 400.
        let (state, session) = session_state(CapturePolicy::default()).await;
        let (status, body) =
            read(&state, awaken_tenancy::DEFAULT_WORKSPACE_ID, &session, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["effective"], "structured", "the open Host default");
        let (foreign, foreign_body) = read(&state, "ws-other", &session, None).await;
        let (unknown, unknown_body) = read(
            &state,
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
            "sesn_missing",
            None,
        )
        .await;
        assert_eq!(foreign, StatusCode::NOT_FOUND);
        assert_eq!(unknown, StatusCode::NOT_FOUND);
        assert_eq!(
            foreign_body["error"]["type"], unknown_body["error"]["type"],
            "one envelope for a foreign and an unknown id"
        );
        let (blank, _) = read(
            &state,
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
            &session,
            Some(" "),
        )
        .await;
        assert_eq!(blank, StatusCode::BAD_REQUEST);
    }
}
