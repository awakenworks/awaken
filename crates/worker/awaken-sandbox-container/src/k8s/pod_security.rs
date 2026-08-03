//! Pure Kubernetes Pod security and network-posture projections.

use k8s_openapi::api::core::v1::{Capabilities, SecurityContext};

/// The egress-posture label value an external platform policy may select on. The
/// label itself is metadata, not enforcement, so this adapter does not advertise
/// network isolation until composition can verify that policy separately.
pub(super) fn egress_label(network: &crate::NetworkMode) -> &'static str {
    match network {
        crate::NetworkMode::Open => "open",
        crate::NetworkMode::None => "restricted",
    }
}

/// The memoryd sidecar's `securityContext` in FUSE mode: it needs `SYS_ADMIN` to
/// mount `/dev/fuse`. Only granted when FUSE is enabled — the copy fallback needs no
/// privilege, so a locked-down (no-FUSE) cluster runs the sidecar unprivileged.
pub(super) fn fuse_sidecar_security_context() -> SecurityContext {
    SecurityContext {
        capabilities: Some(Capabilities {
            add: Some(vec!["SYS_ADMIN".to_string()]),
            drop: None,
        }),
        ..Default::default()
    }
}

/// The hardened `securityContext` for the untrusted agent container: no privilege
/// escalation, every Linux capability dropped.
pub(super) fn hardened_security_context() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        ..Default::default()
    }
}
