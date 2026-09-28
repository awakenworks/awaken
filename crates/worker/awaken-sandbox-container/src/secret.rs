// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: This source implements the repository-controlled credential product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
/// Broker-materialized secret bytes carried only below the provisioning seam.
///
/// The wrapper deliberately redacts `Debug` output so a rendered container plan
/// cannot disclose credential material.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[cfg(any(test, feature = "k8s"))]
    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted secret bytes>")
    }
}
