// @formal entrypoint credential.worker_plaintext_holder_projection_is_exact_and_non_widening
//! Exact projection of an operator-selected trust domain into the sole
//! plaintext-holder boundary implemented by the Worker file resolver.

use awaken_agent_contract::credential::PlaintextHolder;
use awaken_credential_contract::project_worker_plaintext_holder;

/// Preserve the exact opaque trust-domain identity while fixing the boundary
/// to Worker. There is no boundary input and therefore no downgrade/fallback
/// path for the file-backed resolver.
#[must_use]
pub(super) fn exact_worker_holder(trust_domain: impl Into<String>) -> PlaintextHolder {
    let projection = project_worker_plaintext_holder(trust_domain.into());
    PlaintextHolder::new(projection.boundary, projection.trust_domain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::credential::PlaintextBoundary;

    #[test]
    fn concrete_holder_keeps_the_exact_trust_domain() {
        let holder = exact_worker_holder("worker.identity/a");
        assert_eq!(holder.boundary, PlaintextBoundary::Worker);
        assert_eq!(holder.trust_domain.0, "worker.identity/a");
    }
}
