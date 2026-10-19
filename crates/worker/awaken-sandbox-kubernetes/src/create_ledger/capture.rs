// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: Original ledger CAS and fresh capture are fail closed;
// @formal reason: the cause-effect tests cover byte and replay scope, not universal physical closure.
//! Finite fresh capture belongs to the one Create ledger. Legacy retained
//! values keep their original law; this policy never changes physical UID shape.

use awaken_container_contract::CreateReplyProfile;
use http_body_util::{BodyExt, Limited};
use kube::Client;
use kube::client::Body;
use serde::{Deserialize, Serialize};

use awaken_container_contract::TicketResult;

pub(super) const RESULT_BYTES: usize = 8192;
use crate::error::backend;
use awaken_container_contract::RuntimeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FreshCapturePolicy {
    BoundedV1,
}

pub(super) fn checked_size_add(left: usize, right: usize) -> Result<usize, RuntimeError> {
    left.checked_add(right)
        .ok_or_else(|| backend("Create ledger byte count overflow"))
}

pub(super) fn validate_fresh_result(result: &TicketResult) -> Result<(), RuntimeError> {
    if serde_json::to_vec(result).map_err(backend)?.len() > RESULT_BYTES {
        return Err(backend("Create result exceeds its fresh capture policy"));
    }
    Ok(())
}

/// Reuse the same library limiter as Axum's bounded body reader. This helper
/// adapts kube's body only and has no buffer, retry, ticket or permission owner.
pub async fn collect_create_body(
    body: Body,
    profile: CreateReplyProfile,
) -> Result<Vec<u8>, RuntimeError> {
    let bytes = match profile.body_limit() {
        Some(limit) => Limited::new(body, limit)
            .collect()
            .await
            .map_err(|_| backend("Create API response is incomplete or exceeds capture policy"))?
            .to_bytes(),
        None => body.collect_bytes().await.map_err(backend)?,
    };
    Ok(bytes.to_vec())
}

/// Only the existing finite isolation lifetime uses this adapter. Streaming
/// exec/watch clients and explicit legacy recovery keep their original client.
pub(super) fn isolation_client(client: &Client, fresh: bool) -> Client {
    if !fresh {
        return client.clone();
    }
    let namespace = client.default_namespace().to_owned();
    let client = client.clone();
    let service = tower::service_fn(move |request| {
        let client = client.clone();
        async move {
            type Error = Box<dyn std::error::Error + Send + Sync>;
            let response = client
                .send(request)
                .await
                .map_err(|error| Box::new(error) as Error)?;
            let (parts, body) = response.into_parts();
            let bytes = collect_create_body(body, CreateReplyProfile::FreshCreate)
                .await
                .map_err(|error| Box::new(error) as Error)?;
            Ok::<_, Error>(http::Response::from_parts(parts, Body::from(bytes)))
        }
    });
    Client::new(service, namespace)
}

#[cfg(test)]
mod tests;
