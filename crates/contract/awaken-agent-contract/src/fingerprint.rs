//! Deterministic, secret-free equality fingerprints shared by contract values.

use sha2::{Digest, Sha256};

/// Failure to serialize a contract value for canonical JSON hashing.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalJsonHashError {
    #[error("serialize value for canonical JSON hashing: {0}")]
    Serialize(String),
}

/// SHA-256 of one domain-separated, canonical JSON value.
///
/// Object keys are sorted recursively, so adapter insertion order cannot alter
/// one logical request identity. The owner/version domain is length framed with
/// the canonical payload; callers must use a stable domain for each protocol.
pub fn canonical_json_sha256(
    domain: &str,
    value: &impl serde::Serialize,
) -> Result<String, CanonicalJsonHashError> {
    let value = serde_json::to_value(value)
        .map_err(|error| CanonicalJsonHashError::Serialize(error.to_string()))?;
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical);
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain.as_bytes());
    hasher.update((canonical.len() as u64).to_le_bytes());
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    Ok(prefixed_lower_hex("sha256:", &digest))
}

// Both wire formats use the existing hex encoder for fixed-width lowercase
// bytes. Keep prefix assembly here instead of routing through generic fmt.
fn prefixed_lower_hex(prefix: &str, bytes: &[u8]) -> String {
    let mut output = vec![0; prefix.len() + bytes.len() * 2];
    output[..prefix.len()].copy_from_slice(prefix.as_bytes());
    // The encoder's output slice has exactly two bytes per input byte. Both
    // callers have fixed-size digests, so this length cannot overflow.
    hex::encode_to_slice(bytes, &mut output[prefix.len()..])
        .expect("fingerprint hex output has exactly twice the digest length");
    // A UTF-8 prefix followed by ASCII hex is UTF-8; retain the checked safe
    // conversion and its obligation instead of bypassing validation.
    String::from_utf8(output).expect("fingerprint prefix and hexadecimal output are UTF-8")
}

fn write_canonical_json(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            output
                .push_str(&serde_json::to_string(value).expect("a JSON string always serializes"));
        }
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        serde_json::Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key).expect("a JSON object key always serializes"),
                );
                output.push(':');
                write_canonical_json(&values[key], output);
            }
            output.push('}');
        }
    }
}

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
    // Numeric :016x writes the most significant byte first, including zeros.
    prefixed_lower_hex("fnv1a64:", &hash.to_be_bytes())
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

#[cfg(any(test, kani))]
mod hex_verification {
    pub(super) fn assert_wire_bytes(prefix: &str, bytes: &[u8]) {
        // H1 every input byte, including zero and all nibble values, produces
        // exactly two lowercase digits; H2 the prefix and byte order are exact.
        // Decode the actual production output independently: no second encoder
        // or hash implementation is used as the formal oracle. Native witnesses
        // and both unconstrained Kani inputs share these complete assertions.
        let output = super::prefixed_lower_hex(prefix, bytes);
        let wire = output.as_bytes();
        assert_eq!(wire.len(), prefix.len() + bytes.len() * 2);
        for (index, byte) in prefix.bytes().enumerate() {
            assert_eq!(wire[index], byte);
        }
        for (index, byte) in bytes.iter().enumerate() {
            let high = decode_lower_nibble(wire[prefix.len() + index * 2]);
            let low = decode_lower_nibble(wire[prefix.len() + index * 2 + 1]);
            assert_eq!((high << 4) | low, *byte);
        }
    }

    fn decode_lower_nibble(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => panic!("wire digit must be lowercase hexadecimal"),
        }
    }
}

#[cfg(kani)]
mod hex_proofs {
    #[kani::proof]
    #[kani::unwind(18)]
    fn fnv_hex_output_preserves_every_u64() {
        // H1/H2 over all 64 bits: includes every leading-zero count and nibble
        // position. Big-endian bytes preserve the existing numeric wire order.
        // This qualifies only final encoding, not FNV or request_exact itself.
        let hash: u64 = kani::any();
        super::hex_verification::assert_wire_bytes("fnv1a64:", &hash.to_be_bytes());
    }

    #[kani::proof]
    #[kani::unwind(66)]
    fn sha256_hex_output_preserves_every_digest_byte() {
        // H1/H2 over all 256 bits, without restricting bytes to SHA witnesses.
        // Digest encoding qualification is not a proof of SHA or JSON hashing.
        let digest: [u8; 32] = kani::any();
        super::hex_verification::assert_wire_bytes("sha256:", &digest);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn prefixed_hex_preserves_previous_numeric_and_byte_formatting() {
        // H1/H2 compatibility table: N1 zero/max/asymmetric order; N2 each of
        // 16 nibble values at every u64 position (all other nibbles zero); N3
        // SHA-size zero/max/ordered bytes. The previous format expressions are
        // an independent native oracle; the shared inverse oracle also runs.
        for hash in [0, u64::MAX, 1, 0x0102_0304_0506_0708, 0x0123_4567_89ab_cdef]
            .into_iter()
            .chain(
                (0..16)
                    .flat_map(|position| (0..16_u64).map(move |nibble| nibble << (position * 4))),
            )
        {
            let bytes = hash.to_be_bytes();
            super::hex_verification::assert_wire_bytes("fnv1a64:", &bytes);
            assert_eq!(
                super::prefixed_lower_hex("fnv1a64:", &bytes),
                format!("fnv1a64:{hash:016x}")
            );
        }
        for bytes in [
            [0; 32],
            [u8::MAX; 32],
            std::array::from_fn(|index| index as u8),
        ] {
            super::hex_verification::assert_wire_bytes("sha256:", &bytes);
            let old_hex = bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(
                super::prefixed_lower_hex("sha256:", &bytes),
                format!("sha256:{old_hex}")
            );
        }
    }

    #[test]
    fn canonical_sha256_retains_framed_wire_vectors() {
        // Cause/effect rules: V1 empty domain + null; V2 ordered object; V3
        // changed domain; V4 multibyte text + JSON escaping + mixed scalars.
        // Each must retain the complete historical wire bytes. Literals were
        // independently calculated with Python hashlib.sha256 over
        // u64LE(domain byte length) || domain UTF-8 || u64LE(canonical JSON byte
        // length) || canonical JSON UTF-8, before changing the hex formatter.
        for (domain, json, expected) in [
            (
                "",
                "null",
                "sha256:108ef3fd348d3a4f7e252e3e191665345ae66cc4d31411092e8f8f97267811b0",
            ),
            (
                "owner.v1",
                r#"{"a":1,"b":2}"#,
                "sha256:3503e3b08a59ea0260fbae6ad97b6f45b1aca022d1b6b3c22f6a133db06d7581",
            ),
            (
                "owner.v2",
                r#"{"a":1,"b":2}"#,
                "sha256:d65eb543f64bb02563369f53ff84d83e491949711923ee5de65be8dc45203cf7",
            ),
            (
                "domain.unicode",
                r#"["é","\n",0,-1,true,null]"#,
                "sha256:dd2bb5a16ac3bdeeea7edee8a0a800ac4cc22d6644913d29ce15ff852782c4f7",
            ),
        ] {
            let value: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(
                super::canonical_json_sha256(domain, &value).unwrap(),
                expected
            );
        }
    }

    struct SerializationFailure;

    impl serde::Serialize for SerializationFailure {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(
                "fingerprint test serialization failure",
            ))
        }
    }

    #[test]
    fn canonical_sha256_retains_serialization_error() {
        // E1: caller serialization fails before hashing/encoding => the same
        // Serialize error payload and display; no replacement fingerprint.
        let error = super::canonical_json_sha256("owner.v1", &SerializationFailure).unwrap_err();
        let super::CanonicalJsonHashError::Serialize(message) = &error;
        assert_eq!(message, "fingerprint test serialization failure");
        assert_eq!(
            error.to_string(),
            "serialize value for canonical JSON hashing: fingerprint test serialization failure"
        );
    }

    #[test]
    #[should_panic(expected = "contract fingerprint value serializes")]
    fn stable_fingerprint_retains_serialization_panic() {
        // E2: the same failed Serialize input retains FNV's existing expect
        // failure instead of returning a fingerprint or a new error variant.
        let _ = super::stable_fingerprint(&SerializationFailure);
    }

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

    #[test]
    fn canonical_sha256_binds_domain_and_semantics_not_object_order() {
        // Causes: C1 JSON object key insertion order differs; C2 one semantic
        // value differs; C3 the version/owner domain differs. Effects: E1 C1
        // retains one retry identity; E2 C2 and C3 produce distinct identities.
        // Decision rules: H1=C1=>E1, H2=C2=>E2, H3=C3=>E2. This is the one
        // canonical SHA-256 owner for commits and audited management requests.
        let left: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let reordered: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let changed = serde_json::json!({"a": 3, "b": 2});

        let hash = super::canonical_json_sha256("owner.v1", &left).unwrap();
        assert_eq!(
            hash,
            super::canonical_json_sha256("owner.v1", &reordered).unwrap(),
            "H1/E1"
        );
        assert_ne!(
            hash,
            super::canonical_json_sha256("owner.v1", &changed).unwrap(),
            "H2/E2"
        );
        assert_ne!(
            hash,
            super::canonical_json_sha256("owner.v2", &left).unwrap(),
            "H3/E2"
        );
        assert!(hash.starts_with("sha256:"));
    }
}
