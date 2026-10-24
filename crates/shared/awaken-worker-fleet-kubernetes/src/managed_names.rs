//! Managed Pod and continuation-claim names.
//!
//! One owner for the forward Pod/claim projection and its inverses: the Create
//! ledger derives capacity obligations and terminal guards from them, and the
//! Sandbox adapter names the Pods and claims it realizes with them. Callers
//! never reconstruct either prefix independently.

use crate::backend;
use awaken_container_contract::RuntimeError;

/// The longest Kubernetes DNS subdomain name.
pub const K8S_DNS_SUBDOMAIN_MAX_LEN: usize = 253;

/// `id` is the already encoded adapter-local runtime identity.
pub fn pod_name(id: &str) -> String {
    format!("awaken-{id}")
}

/// Deterministic active-state claim for the same encoded Sandbox realization.
/// `awc-` is deliberately shorter than the Pod prefix so even the bounded
/// hashed runtime id remains within the 63-byte DNS-label limit.
pub fn continuation_claim_name(id: &str) -> String {
    format!("awc-{id}")
}

/// Project the canonical retained claim from a managed Pod locator. This is
/// the inverse owned by the same module as the forward Pod/claim projections;
/// callers must not reconstruct either prefix independently.
pub fn continuation_claim_for_pod(pod_name: &str) -> Result<String, RuntimeError> {
    let runtime_id = pod_name
        .strip_prefix("awaken-")
        .ok_or_else(|| backend("invalid managed Kubernetes Pod identity"))?;
    Ok(continuation_claim_name(runtime_id))
}

/// Inverse of the same canonical Pod/PVC projection, used only to derive
/// possible capacity obligations. It grants no resource ownership.
pub fn pod_for_continuation_claim(claim: &str) -> Option<String> {
    let id = claim.strip_prefix("awc-").filter(|id| !id.is_empty())?;
    let pod = pod_name(id);
    (pod.len() <= K8S_DNS_SUBDOMAIN_MAX_LEN).then_some(pod)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_pod_and_claim_names_are_exact_inverses() {
        // Decision table. C1 a managed Pod / foreign identity; C2 a claim with
        // the managed prefix, an empty id, an overlong id or no prefix.
        // N6 managed Pod -> its exact canonical claim; foreign -> refused.
        // N7 the capacity inverse uses this same owner: a managed claim maps
        // back to its Pod; an unclassified, empty or overlong claim maps to none.
        let runtime_id = "sesn-5ffnv1a64-3aa13b83a56e2f77d0";
        let pod = pod_name(runtime_id);
        let claim = continuation_claim_name(runtime_id);
        assert_eq!(continuation_claim_for_pod(&pod).unwrap(), claim, "N6");
        assert!(
            continuation_claim_for_pod("foreign-session").is_err(),
            "N6 foreign"
        );
        assert_eq!(pod_for_continuation_claim(&claim), Some(pod), "N7");
        assert_eq!(pod_for_continuation_claim("unclassified"), None, "N7");
        assert_eq!(pod_for_continuation_claim("awc-"), None, "N7 empty");
        assert_eq!(
            pod_for_continuation_claim(&format!("awc-{}", "x".repeat(253))),
            None,
            "N7 overlong"
        );
    }
}
