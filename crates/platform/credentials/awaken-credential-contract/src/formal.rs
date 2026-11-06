// @formal entrypoint credential.worker_plaintext_holder_projection_is_exact_and_non_widening
use super::project_worker_plaintext_holder;

/// The file-backed Worker resolver preserves the exact opaque trust-domain
/// identity and can project it to no plaintext boundary except Worker.
/// @obligation credential.worker_plaintext_holder_projection_is_exact_and_non_widening
#[kani::proof]
fn worker_plaintext_holder_projection_is_exact_and_non_widening() {
    let trust_domain_identity = kani::any::<u64>();
    let projected = project_worker_plaintext_holder(trust_domain_identity);

    assert_eq!(projected.boundary, super::PlaintextBoundary::Worker);
    assert_eq!(projected.trust_domain, trust_domain_identity);
    assert_ne!(projected.boundary, super::PlaintextBoundary::Workload);
    assert_ne!(projected.boundary, super::PlaintextBoundary::Platform);
}
