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
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
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
