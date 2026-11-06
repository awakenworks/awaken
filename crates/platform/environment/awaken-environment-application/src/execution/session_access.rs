// @formal surface requirement=session.managed_worker.work_authority
// @formal reason: Static owner: the single issuer and verifier derivation of the per-Session Work bearer
// @formal reason: returned by poll; it generates cleartext from the platform CSPRNG and derives the v1
// @formal reason: digest, and persists nothing. Dynamic path: the WorkQueue binds only that digest to the
// @formal reason: new lease epoch in the claim transaction and later resolves a presented digest to the
// @formal reason: exact live lease, so reclaim, stop or retirement fences the bearer; a known-answer test
// @formal reason: pins digest compatibility with already stored leases.
//! Issuance of per-Session Work bearers.
//!
//! The Environment execution application is the one issuer and verifier of
//! the bearer an official Environment Worker receives from `poll`. It generates
//! the cleartext from the platform CSPRNG and derives the digest; the WorkQueue
//! port only binds that digest to the lease epoch it writes in the same
//! transaction and later resolves a presented digest to that lease.

use awaken_agent_contract::RedactedString;
use awaken_executable_environment_contract::SessionAccessDigest;
use awaken_iam_core::{EntropySource, OsEntropy};
use base64::Engine as _;
use sha2::{Digest, Sha256};

const SESSION_TOKEN_BYTES: usize = 32;
/// Wire prefix of the bearer. Protocol guards route only this prefix to the
/// Work-session authenticator.
pub const SESSION_TOKEN_PREFIX: &str = "sk-ant-req-";
/// Domain separator of the persisted digest. Changing it would orphan every
/// digest already stored beside a live lease.
const DIGEST_DOMAIN: &[u8] = b"awaken-work-session-token-v1\0";

/// A freshly minted bearer and its digest. The cleartext leaves the process
/// only in the `poll` response of the claim that bound the digest.
pub(super) struct MintedSessionAccess {
    pub(super) cleartext: RedactedString,
    pub(super) digest: SessionAccessDigest,
}

pub(super) fn mint_session_access() -> MintedSessionAccess {
    let mut bytes = [0_u8; SESSION_TOKEN_BYTES];
    OsEntropy.fill_bytes(&mut bytes);
    let cleartext = RedactedString::new(format!(
        "{SESSION_TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ));
    let digest = session_access_digest(&cleartext);
    MintedSessionAccess { cleartext, digest }
}

pub(super) fn session_access_digest(presented: &RedactedString) -> SessionAccessDigest {
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    digest.update(presented.expose_secret().as_bytes());
    SessionAccessDigest::from_hex(format!("{:x}", digest.finalize()))
        .expect("SHA-256 renders 64 lowercase hexadecimal characters")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_the_persisted_v1_derivation_and_minting_is_unique() {
        // C1 a bearer issued before issuance moved out of the WorkQueue; C2 a
        // freshly minted bearer. E1 the digest of C1 is byte-identical to the
        // value earlier releases stored in `session_token_sha256` (known answer
        // computed independently), so live leases keep authenticating; E2 C2
        // carries the wire prefix, 32 bytes of entropy, and its own digest.
        assert_eq!(
            session_access_digest(&RedactedString::new("sk-ant-req-known-answer")).as_str(),
            "850955f9fd04f242e1c9e0356a320d5fd2b3045306fea01d74b2e4e0fbecffd3",
            "E1"
        );
        let first = mint_session_access();
        let second = mint_session_access();
        let secret = first.cleartext.expose_secret();
        assert!(secret.starts_with(SESSION_TOKEN_PREFIX), "E2 prefix");
        assert_eq!(
            secret.len(),
            SESSION_TOKEN_PREFIX.len() + 43,
            "E2 entropy width"
        );
        assert_eq!(first.digest, session_access_digest(&first.cleartext), "E2");
        assert_ne!(first.digest, second.digest, "E2 unique");
    }
}
