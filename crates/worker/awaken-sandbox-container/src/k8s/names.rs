use super::{RuntimeError, backend};

const K8S_DNS_LABEL_MAX_LEN: usize = 63;
const K8S_DNS_SUBDOMAIN_MAX_LEN: usize = 253;
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// Injectively encode the opaque Sandbox scope as an adapter-local Kubernetes
/// identity. Lowercase ASCII letters/digits pass through and every other UTF-8
/// byte becomes `-hh`. Since `-` itself is escaped, the transform is reversible
/// and cannot normalize two scopes to one name. An overlong scope fails instead
/// of being truncated or replaced by a probabilistic hash.
pub(super) fn k8s_runtime_id(scope: &str) -> Result<String, RuntimeError> {
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

    let pod = pod_name(&encoded);
    let configmap = configmap_name(&encoded, usize::MAX);
    let secret = credential_secret_name(&encoded, usize::MAX);
    if pod.len() > K8S_DNS_LABEL_MAX_LEN {
        return Err(backend(format!(
            "sandbox scope cannot be represented as a Kubernetes label: encoded Pod name is {} bytes (max {K8S_DNS_LABEL_MAX_LEN})",
            pod.len()
        )));
    }
    if configmap.len() > K8S_DNS_SUBDOMAIN_MAX_LEN || secret.len() > K8S_DNS_SUBDOMAIN_MAX_LEN {
        return Err(backend(
            "sandbox scope cannot be represented in projected Kubernetes resource names",
        ));
    }
    Ok(encoded)
}

/// `id` is the already encoded adapter-local runtime identity.
pub(super) fn pod_name(id: &str) -> String {
    format!("awaken-{id}")
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
         * deterministic RFC-1123-safe identity; E2 round-trip every accepted scope
         * exactly, proving injectivity; E3 use one Pod identity for all names and
         * keep them within 63/253 bytes; E4 reject invalid bounds. Rules:
         * N1 C1|C3=>E1+E2; N2 C1+C2=>E2; N3 C1+C4=>E3; N4 C5=>E4.
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
        let configmap = configmap_name(&runtime_id, usize::MAX);
        let secret = credential_secret_name(&runtime_id, usize::MAX);
        assert!(is_dns_name(&pod, K8S_DNS_LABEL_MAX_LEN), "N3 pod: {pod}");
        assert!(
            is_dns_name(&configmap, K8S_DNS_SUBDOMAIN_MAX_LEN),
            "N3 ConfigMap: {configmap}"
        );
        assert!(
            is_dns_name(&secret, K8S_DNS_SUBDOMAIN_MAX_LEN),
            "N3 Secret: {secret}"
        );
        assert_eq!(cfg_owner_label(&runtime_id), pod, "N3 cleanup owner");

        for scope in [String::new(), "x".repeat(57)] {
            assert!(k8s_runtime_id(&scope).is_err(), "N4: {scope}");
        }
    }
}
