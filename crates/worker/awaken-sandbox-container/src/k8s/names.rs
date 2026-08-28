use super::{RuntimeError, backend};

const K8S_DNS_LABEL_MAX_LEN: usize = 63;
const K8S_DNS_SUBDOMAIN_MAX_LEN: usize = 253;
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// Encode the opaque Sandbox scope as an adapter-local Kubernetes identity.
/// Lowercase ASCII letters/digits pass through and every other UTF-8 byte becomes
/// `-hh`. Short identities therefore remain reversible and collision-free. A
/// scope that cannot fit a DNS label uses the complete BLAKE3 digest in unpadded
/// lowercase base32; Kubernetes has a finite name space, so a cryptographic
/// content identity is the only bounded representation for arbitrary scopes.
pub(crate) fn k8s_runtime_id(scope: &str) -> Result<String, RuntimeError> {
    if scope.is_empty() {
        return Err(backend("sandbox scope cannot be empty"));
    }
    let mut encoded = String::with_capacity(scope.len().saturating_mul(3));
    for byte in scope.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            encoded.push(char::from(byte));
        } else {
            encoded.push('-');
            encoded.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
        }
    }

    if pod_name(&encoded).len() > K8S_DNS_LABEL_MAX_LEN {
        encoded = format!("h-{}", base32(blake3::hash(scope.as_bytes()).as_bytes()));
    }

    let pod = pod_name(&encoded);
    let configmap = configmap_name(&encoded, usize::MAX);
    let secret = credential_secret_name(&encoded, usize::MAX);
    debug_assert!(pod.len() <= K8S_DNS_LABEL_MAX_LEN);
    if configmap.len() > K8S_DNS_SUBDOMAIN_MAX_LEN || secret.len() > K8S_DNS_SUBDOMAIN_MAX_LEN {
        return Err(backend(
            "sandbox scope cannot be represented in projected Kubernetes resource names",
        ));
    }
    Ok(encoded)
}

fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut output = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut accumulator = 0_u16;
    let mut bits = 0_u8;
    for &byte in bytes {
        accumulator = (accumulator << 8) | u16::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            output.push(char::from(
                ALPHABET[usize::from((accumulator >> bits) & 0x1f)],
            ));
        }
        accumulator &= if bits == 0 { 0 } else { (1_u16 << bits) - 1 };
    }
    if bits > 0 {
        output.push(char::from(
            ALPHABET[usize::from((accumulator << (5 - bits)) & 0x1f)],
        ));
    }
    output
}

/// `id` is the already encoded adapter-local runtime identity.
pub(crate) fn pod_name(id: &str) -> String {
    format!("awaken-{id}")
}

/// Deterministic active-state claim for the same encoded Sandbox realization.
/// `awc-` is deliberately shorter than the Pod prefix so even the bounded
/// hashed runtime id remains within the 63-byte DNS-label limit.
pub(super) fn continuation_claim_name(id: &str) -> String {
    format!("awc-{id}")
}

/// Deterministic ConfigMap name for the i-th inline-content mount.
pub(super) fn configmap_name(id: &str, i: usize) -> String {
    format!("{}-cfg-{i}", pod_name(id))
}

pub(super) fn credential_secret_name(id: &str, i: usize) -> String {
    format!("{}-credential-{i}", pod_name(id))
}

/// Cleanup label value and public runtime handle are the Pod name.
pub(super) fn cfg_owner_label(id: &str) -> String {
    pod_name(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_dns_name(value: &str, max_len: usize) -> bool {
        !value.is_empty()
            && value.len() <= max_len
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
    }

    fn decode_runtime_id(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut decoded = Vec::new();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if bytes[cursor] == b'-' {
                decoded.push(u8::from_str_radix(&value[cursor + 1..cursor + 3], 16).unwrap());
                cursor += 3;
            } else {
                decoded.push(bytes[cursor]);
                cursor += 1;
            }
        }
        String::from_utf8(decoded).unwrap()
    }

    #[test]
    fn opaque_scope_has_one_collision_safe_k8s_resource_identity() {
        /*
         * Kubernetes-name cause/effect decision table. Causes: C1 a managed
         * Session scope contains `_`/`:`; C2 two scopes would collide under lossy
         * replacement; C3 a scope contains uppercase or Unicode; C4 every derived
         * resource/label name and the maximum index must fit Kubernetes limits;
         * C5 scope is empty or its reversible form is overlong. Effects: E1 emit a
         * deterministic RFC-1123-safe identity; E2 round-trip every short scope
         * exactly, proving injectivity; E3 use one Pod identity for all names and
         * keep them within 63/253 bytes; E4 reject empty scope; E5 map arbitrary
         * long scopes to a full collision-resistant digest. Rules: N1 C1|C3=>E1+E2;
         * N2 C1+C2=>E2; N3 C1+C4=>E3; N4 empty C5=>E4; N5 long C5=>E3+E5.
         */
        let session = "sesn_fnv1a64:a13b83a56e2f77d0";
        let runtime_id = k8s_runtime_id(session).unwrap();
        assert_eq!(runtime_id, "sesn-5ffnv1a64-3aa13b83a56e2f77d0", "N1");
        assert_eq!(runtime_id, k8s_runtime_id(session).unwrap(), "N1 stable");
        assert_ne!(
            k8s_runtime_id("a:b").unwrap(),
            k8s_runtime_id("a-b").unwrap(),
            "N2"
        );
        for scope in [session, "run-9", "Session", "会话:一"] {
            let encoded = k8s_runtime_id(scope).unwrap();
            assert_eq!(decode_runtime_id(&encoded), scope, "N1/N2: {scope}");
        }

        let pod = pod_name(&runtime_id);
        let claim = continuation_claim_name(&runtime_id);
        let configmap = configmap_name(&runtime_id, usize::MAX);
        let secret = credential_secret_name(&runtime_id, usize::MAX);
        assert!(is_dns_name(&pod, K8S_DNS_LABEL_MAX_LEN), "N3 pod: {pod}");
        assert!(
            is_dns_name(&claim, K8S_DNS_LABEL_MAX_LEN),
            "N3 continuation PVC: {claim}"
        );
        assert!(
            is_dns_name(&configmap, K8S_DNS_SUBDOMAIN_MAX_LEN),
            "N3 ConfigMap: {configmap}"
        );
        assert!(
            is_dns_name(&secret, K8S_DNS_SUBDOMAIN_MAX_LEN),
            "N3 Secret: {secret}"
        );
        assert_eq!(cfg_owner_label(&runtime_id), pod, "N3 cleanup owner");

        assert!(k8s_runtime_id("").is_err(), "N4");

        let long_scope = format!(
            "state-entry:issue_{}:plan:5",
            "0718bf2ab7d756f8bc85db4cc92b0ae77f83acc55626e9facfe8d8ab80fee99e"
        );
        let long_id = k8s_runtime_id(&long_scope).unwrap();
        assert!(long_id.starts_with("h-"), "N5: {long_id}");
        assert_eq!(long_id.len(), 54, "N5 full BLAKE3 base32");
        assert_eq!(long_id, k8s_runtime_id(&long_scope).unwrap(), "N5 stable");
        assert_ne!(
            long_id,
            k8s_runtime_id(&format!("{long_scope}-other")).unwrap(),
            "N5 distinct long scopes"
        );
        assert!(
            is_dns_name(&pod_name(&long_id), K8S_DNS_LABEL_MAX_LEN),
            "N5 bounded Pod name"
        );
    }
}
