// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: The original shared metadata field law is not UID, execution or cleanup authority.
//! Shared original-Create metadata names and their existing field-only law.

use crate::CreateTicketRequest;

pub const ISOLATION_ID_ANNOTATION: &str = "awaken.dev/create-isolation-id";
pub const REQUEST_DIGEST_ANNOTATION: &str = "awaken.dev/create-request-digest";
pub const ISOLATION_LABEL: &str = "awaken.dev/create-isolation";
/// Retains an original Pod until its exact Create result and terminal disposal
/// have been joined. The existing Pod-finalizer CAS helper owns release.
pub const ORIGINAL_GUARD_FINALIZER: &str = "awaken.dev/create-result-guard";
/// Original-request field law shared by typed ledger observations, provider
/// reservations and broker JSON results. UID admission, nonce presence and
/// isolation purpose remain with each caller; this grants no effect authority.
#[must_use]
pub fn original_request_metadata_matches(
    request: &CreateTicketRequest,
    nonce: &str,
    name: Option<&str>,
    namespace: Option<&str>,
    observed_nonce: Option<&str>,
    request_digest: Option<&str>,
    not_deleting: bool,
) -> bool {
    name == Some(request.name.as_str())
        && namespace == Some(request.namespace.as_str())
        && observed_nonce == Some(nonce)
        && request_digest == Some(request.digest.as_str())
        && not_deleting
}
