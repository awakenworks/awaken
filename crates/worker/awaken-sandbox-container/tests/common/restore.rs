use awaken_provisioning_contract as pc;

pub(crate) fn exact_restore_spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    }
}

pub(crate) fn exact_restore_request(scope: &str) -> pc::SandboxRestoreRequest {
    pc::SandboxRestoreRequest {
        workspace_id: "restore-integration-workspace".into(),
        session_id: scope.into(),
        effect_id: "blake3:0000000000000000000000000000000000000000000000000000000000000003".into(),
        generation_id: "restore-integration-generation".into(),
        checkpoint: pc::SandboxCheckpointRef {
            id: "restore-integration-checkpoint".into(),
            format: "provider-owned-checkpoint".into(),
            digest: "restore-integration-digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: "restore-integration-environment".into(),
            base_image_fingerprint: "restore-integration-image".into(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: "restore-integration-suspend".into(),
        },
    }
}
