//! Stable, secret-free fingerprints for Session aggregate values.

/// Serialize one value canonically enough for repository idempotency and compute
/// FNV-1a. This is an equality fingerprint, not an authorization or secrecy
/// primitive; security-sensitive payloads use their owning domain's digest.
#[must_use]
pub fn stable_fingerprint(value: &impl serde::Serialize) -> String {
    let encoded = serde_json::to_vec(value).expect("Session fingerprint value serializes");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in encoded {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}
