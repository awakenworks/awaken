use super::*;

/// Wire written by the closed payload enum that previously lived in the
/// neutral contract, generated from that implementation (never hand-edited).
const HISTORICAL_WIRE: &[(&str, &str)] = &[
    (
        "local_v1",
        r#"{"sandbox_id":"s-local","payload":{"schema":"local_v1","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"continuation_excluded_paths":["/mnt/cache"],"deny_tool_egress":true}}"#,
    ),
    (
        "local_v2",
        r#"{"sandbox_id":"s-local","payload":{"schema":"local_v2","previous":{"outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"continuation_excluded_paths":["/mnt/cache"],"deny_tool_egress":true},"realization_fingerprint":"fp-realization","effect_fence":{"operation_id":"op-1","owner":"owner-1","runtime_incarnation":"runtime-1","epoch":7,"expires_at_unix_ms":1900000000000},"physical_incarnation":"inc-local","owned_paths":["/workspace/repo","/mnt/memory"]},"memory_materializations":[{"store_id":"memory-a","mount_path":"/mnt/memory","heads":[{"path":"notes.md","id":"head-1","version":3,"content_sha256":"sha-1"}]}]}"#,
    ),
    (
        "bubblewrap_v1",
        r#"{"sandbox_id":"s-ns","payload":{"schema":"bubblewrap_v1","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"network":{"mode":"allowlist","hosts":["example.com"]},"control_services":["repository_git_credential"]}}"#,
    ),
    (
        "bubblewrap_v2",
        r#"{"sandbox_id":"s-ns","payload":{"schema":"bubblewrap_v2","previous":{"outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"network":{"mode":"allowlist","hosts":["example.com"]},"control_services":["repository_git_credential"]},"realization_fingerprint":"fp-realization","effect_fence":{"operation_id":"op-1","owner":"owner-1","runtime_incarnation":"runtime-1","epoch":7,"expires_at_unix_ms":1900000000000},"physical_incarnation":"inc-ns","owned_paths":["/workspace/repo"]}}"#,
    ),
    (
        "seatbelt_v1",
        r#"{"sandbox_id":"s-ns","payload":{"schema":"seatbelt_v1","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"network":{"mode":"allowlist","hosts":["example.com"]},"control_services":["repository_git_credential"]}}"#,
    ),
    (
        "seatbelt_v2",
        r#"{"sandbox_id":"s-ns","payload":{"schema":"seatbelt_v2","previous":{"outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"network":{"mode":"allowlist","hosts":["example.com"]},"control_services":["repository_git_credential"]},"realization_fingerprint":"fp-realization","effect_fence":{"operation_id":"op-1","owner":"owner-1","runtime_incarnation":"runtime-1","epoch":7,"expires_at_unix_ms":1900000000000},"physical_incarnation":"inc-ns","owned_paths":[]}}"#,
    ),
];

fn golden(name: &str) -> pc::SandboxHandle {
    let text = HISTORICAL_WIRE
        .iter()
        .find_map(|(row, text)| (*row == name).then_some(*text))
        .unwrap();
    serde_json::from_str(text).unwrap()
}

fn text(name: &str) -> &'static str {
    HISTORICAL_WIRE
        .iter()
        .find_map(|(row, text)| (*row == name).then_some(*text))
        .unwrap()
}

fn env() -> Vec<pc::EnvVar> {
    serde_json::from_value(serde_json::json!([
        {"name": "TZ", "value": {"kind": "inline", "value": "UTC"}, "visibility": "process"},
        {"name": "TOKEN", "value": {"kind": "secret", "reference": "ref-1"}, "visibility": "egress_only"}
    ]))
    .unwrap()
}

fn fence() -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new("op-1", "owner-1", "runtime-1", 7, 1_900_000_000_000).unwrap()
}

fn fingerprint() -> pc::SandboxRealizationFingerprint {
    serde_json::from_value(serde_json::json!("fp-realization")).unwrap()
}

fn local_v1() -> LocalSandboxHandleV1 {
    LocalSandboxHandleV1 {
        outputs_path: "/mnt/session/outputs".into(),
        base_env: env(),
        continuation_excluded_paths: vec!["/mnt/cache".into()],
        deny_tool_egress: true,
    }
}

fn namespace_v1() -> NamespaceSandboxHandleV1 {
    NamespaceSandboxHandleV1 {
        outputs_path: "/mnt/session/outputs".into(),
        base_env: env(),
        network: pc::NetworkPolicy::Allowlist {
            hosts: vec!["example.com".into()],
        },
        control_services: [SandboxControlServiceKind::RepositoryGitCredential]
            .into_iter()
            .collect(),
    }
}

fn namespace_v2(owned_paths: Vec<String>) -> NamespaceSandboxHandleV2 {
    NamespaceSandboxHandleV2 {
        previous: namespace_v1(),
        realization_fingerprint: fingerprint(),
        effect_fence: fence(),
        physical_incarnation: "inc-ns".into(),
        owned_paths,
    }
}

#[test]
fn historical_filesystem_handles_decode_to_the_same_typed_payloads_and_bytes() {
    // Old format -> new model -> re-encode. C1 each historical Local/Namespace
    // shape. E1 the codec decodes the exact typed locator; E2 V2 evidence is
    // exposed through the codec and the neutral envelope; E3 the typed writer
    // produces byte-identical wire, so bindings written before and after the
    // move compare equal.
    let memory = pc::MemoryMaterializationEvidence::new(
        "memory-a",
        "/mnt/memory",
        vec![pc::MemoryMaterializationHead {
            path: "notes.md".into(),
            id: "head-1".into(),
            version: 3,
            content_sha256: "sha-1".into(),
        }],
    )
    .unwrap();
    let written = [
        ("local_v1", pc::SandboxHandle::local("s-local", local_v1())),
        (
            "local_v2",
            pc::SandboxHandle::local_v2(
                "s-local",
                LocalSandboxHandleV2 {
                    previous: local_v1(),
                    realization_fingerprint: fingerprint(),
                    effect_fence: fence(),
                    physical_incarnation: "inc-local".into(),
                    owned_paths: vec!["/workspace/repo".into(), "/mnt/memory".into()],
                },
            )
            .with_memory_materializations(vec![memory])
            .unwrap(),
        ),
        (
            "bubblewrap_v1",
            pc::SandboxHandle::namespace(NamespaceProviderKind::Bubblewrap, "s-ns", namespace_v1()),
        ),
        (
            "bubblewrap_v2",
            pc::SandboxHandle::namespace_v2(
                NamespaceProviderKind::Bubblewrap,
                "s-ns",
                namespace_v2(vec!["/workspace/repo".into()]),
            ),
        ),
        (
            "seatbelt_v1",
            pc::SandboxHandle::namespace(NamespaceProviderKind::Seatbelt, "s-ns", namespace_v1()),
        ),
        (
            "seatbelt_v2",
            pc::SandboxHandle::namespace_v2(
                NamespaceProviderKind::Seatbelt,
                "s-ns",
                namespace_v2(Vec::new()),
            ),
        ),
    ];
    assert_eq!(written.len(), HISTORICAL_WIRE.len());
    for (name, handle) in written {
        assert_eq!(
            serde_json::to_string(&handle).unwrap(),
            text(name),
            "E3 {name}"
        );
        let decoded = golden(name);
        assert_eq!(decoded, handle, "E1 {name}");
        let current = name.ends_with("_v2");
        assert_eq!(
            decoded.filesystem_effect_fence().unwrap().cloned(),
            current.then(fence),
            "E2 {name}"
        );
        assert_eq!(
            decoded.realization_fingerprint().cloned(),
            current.then(fingerprint),
            "E2 {name}"
        );
    }
    assert_eq!(
        golden("local_v1").local_payload().unwrap(),
        local_v1(),
        "E1"
    );
    assert_eq!(
        golden("local_v2").local_payload().unwrap(),
        local_v1(),
        "E1"
    );
    assert_eq!(
        golden("local_v2")
            .filesystem_physical_incarnation()
            .unwrap()
            .as_deref(),
        Some("inc-local"),
        "E2"
    );
    assert_eq!(
        golden("local_v2").owned_paths().unwrap(),
        ["/workspace/repo", "/mnt/memory"],
        "E2"
    );
    for (name, kind) in [
        ("bubblewrap_v1", NamespaceProviderKind::Bubblewrap),
        ("bubblewrap_v2", NamespaceProviderKind::Bubblewrap),
        ("seatbelt_v1", NamespaceProviderKind::Seatbelt),
        ("seatbelt_v2", NamespaceProviderKind::Seatbelt),
    ] {
        assert_eq!(
            golden(name).namespace_payload(kind).unwrap(),
            namespace_v1(),
            "E1 {name}"
        );
    }
    assert_eq!(
        golden("local_v1")
            .filesystem_physical_incarnation()
            .unwrap(),
        None
    );
}

#[test]
fn legacy_defaults_normalize_only_through_the_owning_codec() {
    // C1 a pre-default local V1 payload omits `deny_tool_egress`. E1 the neutral
    // envelope keeps the stored bytes unchanged; E2 the codec decodes the
    // historical default; E3 a provider-rebuilt handle writes the normalized
    // shape, exactly as the former typed decoder re-encoded it.
    let stored = r#"{"sandbox_id":"s","payload":{"schema":"local_v1","outputs_path":"/o","base_env":[],"continuation_excluded_paths":[]}}"#;
    let decoded: pc::SandboxHandle = serde_json::from_str(stored).unwrap();
    assert_eq!(serde_json::to_string(&decoded).unwrap(), stored, "E1");
    let payload = decoded.local_payload().unwrap();
    assert!(!payload.deny_tool_egress, "E2");
    assert_eq!(
        serde_json::to_string(&pc::SandboxHandle::local("s", payload)).unwrap(),
        r#"{"sandbox_id":"s","payload":{"schema":"local_v1","outputs_path":"/o","base_env":[],"continuation_excluded_paths":[],"deny_tool_egress":false}}"#,
        "E3"
    );
}

#[test]
fn filesystem_codec_rejects_foreign_and_malformed_payloads() {
    // Rejection table; each row must fail before any provider effect.
    let foreign_kind = golden("bubblewrap_v2");
    assert!(
        foreign_kind.local_payload().is_err(),
        "R1 local reads Namespace"
    );
    assert!(
        foreign_kind
            .namespace_payload(NamespaceProviderKind::Seatbelt)
            .is_err(),
        "R2 Seatbelt reads Bubblewrap"
    );
    let unmanaged = pc::SandboxHandle::new("local", "s");
    assert!(unmanaged.local_payload().is_err(), "R3 unmanaged");
    assert!(unmanaged.filesystem_effect_fence().is_err(), "R3 unmanaged");
    let container: pc::SandboxHandle = serde_json::from_str(
        r#"{"sandbox_id":"s","payload":{"schema":"container_v1","container_id":"c"}}"#,
    )
    .unwrap();
    assert!(
        matches!(
            container.filesystem_effect_fence(),
            Err(pc::SandboxError::Unsupported(_))
        ),
        "R4 another provider"
    );
    let edit = |name: &str, edit: &dyn Fn(&mut serde_json::Value)| {
        let mut wire: serde_json::Value = serde_json::from_str(text(name)).unwrap();
        edit(&mut wire["payload"]);
        serde_json::from_value::<pc::SandboxHandle>(wire)
    };
    let missing_locator = edit("bubblewrap_v1", &|payload| {
        payload.as_object_mut().unwrap().remove("network");
    })
    .unwrap();
    assert!(
        missing_locator
            .namespace_payload(NamespaceProviderKind::Bubblewrap)
            .is_err(),
        "R5 corrupt locator"
    );
    let missing_fence = edit("local_v2", &|payload| {
        payload.as_object_mut().unwrap().remove("effect_fence");
    })
    .unwrap();
    assert!(
        missing_fence.filesystem_effect_fence().is_err(),
        "R6 V2 without fence"
    );
    let empty_incarnation = edit("local_v2", &|payload| {
        payload["physical_incarnation"] = serde_json::json!(" ");
    })
    .unwrap();
    assert!(
        matches!(
            empty_incarnation.filesystem_physical_incarnation(),
            Err(pc::SandboxError::Invalid(_))
        ),
        "R7 empty current incarnation"
    );
    let future = edit("local_v2", &|payload| {
        payload["schema"] = serde_json::json!("local_v3");
    })
    .unwrap();
    assert!(future.local_payload().is_err(), "R8 unknown version");
    assert!(
        future.filesystem_effect_fence().is_err(),
        "R8 unknown version"
    );
    assert!(
        edit("local_v2", &|payload| {
            payload
                .as_object_mut()
                .unwrap()
                .remove("realization_fingerprint");
        })
        .is_err(),
        "R9 current V2 never accepts missing immutable realization evidence"
    );
}
