//! Deterministic, secret-free equality fingerprints shared by contract values.

/// Serialize one value deterministically and compute FNV-1a. This is an
/// equality/corruption fingerprint, not an authorization, secrecy, or MAC
/// primitive; trust decisions must still validate their owning policy facts.
#[must_use]
pub fn stable_fingerprint(value: &impl serde::Serialize) -> String {
    let encoded = serde_json::to_vec(value).expect("contract fingerprint value serializes");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in encoded {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn equal_values_have_equal_fingerprints_and_changed_values_do_not() {
        assert_eq!(
            super::stable_fingerprint(&(1_u64, "a")),
            super::stable_fingerprint(&(1_u64, "a"))
        );
        assert_ne!(
            super::stable_fingerprint(&(1_u64, "a")),
            super::stable_fingerprint(&(1_u64, "b"))
        );
    }
}
