// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Candidate routing cannot authorize a Run or its claim.
/// The MD5 byte is used only to distribute Session candidates over fixed shards.
/// PostgreSQL computes the same byte in the dispatch generated column.
pub fn dispatch_shard(scope: &str, session: &str) -> u8 {
    use md5::{Digest, Md5};
    let mut hash = Md5::new();
    hash.update(scope.as_bytes());
    hash.update([31]);
    hash.update(session.as_bytes());
    hash.finalize()[0]
}

/// Narrow the existing claim request without changing its replay identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimSearch {
    Any,
    Replay,
    Shard(u8),
}
