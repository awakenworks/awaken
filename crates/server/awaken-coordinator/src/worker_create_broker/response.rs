// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: The existing broker sends the same exact bytes checked before capture; this is no cleanup receipt.
//! The exact serialized response crosses the original private HTTP route.
use awaken_container_contract::K8sCreateResponse;
use awaken_container_contract::{CreateReplyProfile, K8S_CREATE_CAPTURE_HEADER};
use awaken_session_execution_contract::ApplicationError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(super) struct BrokerCreateReply {
    pub(super) body: Vec<u8>,
    profile: CreateReplyProfile,
}

impl BrokerCreateReply {
    pub(super) fn new(
        response: K8sCreateResponse,
        profile: CreateReplyProfile,
    ) -> Result<Self, ApplicationError> {
        let body = profile.encode_body(&response).map_err(|_| {
            ApplicationError::unavailable("complete Create response exceeds its capture policy")
        })?;
        Ok(Self { body, profile })
    }

    pub(super) fn from_encoded(body: Vec<u8>, profile: CreateReplyProfile) -> Self {
        Self { body, profile }
    }

    pub(super) fn into_http_response(self) -> Response {
        (
            StatusCode::OK,
            [
                ("content-type", "application/json"),
                (K8S_CREATE_CAPTURE_HEADER, self.profile.header_value()),
            ],
            self.body,
        )
            .into_response()
    }
}
