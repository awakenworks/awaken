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

/// Compute one deterministic, collision-resistant, secret-free identity.
///
/// The caller owns the domain and the ordered components. Every component is
/// byte-length framed, so different tuple boundaries cannot alias. This is an
/// equality identity, not an authorization, secrecy, or MAC primitive.
#[must_use]
pub fn collision_resistant_fingerprint(domain: &str, components: &[&[u8]]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"awaken-collision-resistant-fingerprint-v1\0");
    for component in std::iter::once(domain.as_bytes()).chain(components.iter().copied()) {
        hash.update(&(component.len() as u64).to_be_bytes());
        hash.update(component);
    }
    format!("blake3:{}", hash.finalize().to_hex())
}

/// Return the complete lowercase BLAKE3 digest from the canonical wire form.
///
/// Parsing proves syntax only. It does not establish authority or provenance.
#[must_use]
pub fn collision_resistant_fingerprint_digest(value: &str) -> Option<&str> {
    let digest = value.strip_prefix("blake3:")?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(digest)
}

#[cfg(test)]
mod tests {
    // Cause/effect table: R1 same domain+framed components => same canonical
    // digest; R2 domain differs => different digest; R3 byte concatenation is
    // equal but component boundaries differ => different digest; R4 malformed
    // prefix/length/case/alphabet => parser rejects. Effects are equality and
    // canonical syntax only; no row grants authorization.
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

    #[test]
    fn collision_resistant_fingerprints_are_domain_separated_and_length_framed() {
        let first = super::collision_resistant_fingerprint("domain-a", &[b"ab", b"c"]);
        assert_eq!(
            first,
            super::collision_resistant_fingerprint("domain-a", &[b"ab", b"c"]),
            "R1"
        );
        assert_ne!(
            first,
            super::collision_resistant_fingerprint("domain-b", &[b"ab", b"c"]),
            "R2"
        );
        assert_ne!(
            first,
            super::collision_resistant_fingerprint("domain-a", &[b"a", b"bc"]),
            "R3"
        );
        assert_eq!(
            super::collision_resistant_fingerprint_digest(&first),
            Some(&first["blake3:".len()..]),
            "R1"
        );
    }

    #[test]
    fn collision_resistant_fingerprint_parser_rejects_noncanonical_forms() {
        for malformed in [
            "fnv1a64:0000000000000000",
            "blake3:00",
            "blake3:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "blake3:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
        ] {
            assert_eq!(
                super::collision_resistant_fingerprint_digest(malformed),
                None,
                "R4: {malformed}"
            );
        }
    }
}
