use super::*;
use serde::Deserialize;

/// Wire produced by the closed typed payload enum this module replaced
/// (generated from that implementation, never hand-edited). Every row must
/// decode into the neutral model and re-encode byte-for-byte.
const HISTORICAL_WIRE: &[(&str, &str)] = &[
    (
        "unmanaged",
        r#"{"sandbox_id":"s-unmanaged","payload":{"schema":"unmanaged","provider_kind":"test-tar"}}"#,
    ),
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
    (
        "container_v1_docker",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v1","container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"]}}"#,
    ),
    (
        "container_v1_k8s_legacy",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v1","container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"kubernetes_continuation","claim_uid":"claim-a"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]}}"#,
    ),
    (
        "container_v2_k8s",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v2","previous":{"container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"kubernetes_continuation_v2","pod_uid":"pod-uid-a","claim_uid":"claim-a"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]},"adoption_fingerprint":"fp-adoption","realization_fingerprint":"fp-realization","owned_paths":["/workspace/repo"]},"memory_materializations":[{"store_id":"memory-a","mount_path":"/mnt/memory","heads":[{"path":"notes.md","id":"head-1","version":3,"content_sha256":"sha-1"}]}]}"#,
    ),
    (
        "container_v2_k8s_no_claim",
        r#"{"sandbox_id":"s-c","payload":{"schema":"container_v2","previous":{"container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"kubernetes_continuation_v2","pod_uid":"pod-uid-b"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]},"adoption_fingerprint":"fp-adoption","realization_fingerprint":"fp-realization","owned_paths":[]}}"#,
    ),
    (
        "container_v2_host_bind",
        r#"{"sandbox_id":"s-c","restoration":{"effect_id":"effect-a","generation_id":"generation-a","checkpoint_id":"checkpoint-a","checkpoint_digest":"sha256:digest-a","sandbox_spec_fingerprint":"spec-a","checkpoint_exclusions_fingerprint":"exclusions-a"},"payload":{"schema":"container_v2","previous":{"container_id":"container-a","outputs_path":"/mnt/session/outputs","base_env":[{"name":"TZ","value":{"kind":"inline","value":"UTC"},"visibility":"process"},{"name":"TOKEN","value":{"kind":"secret","reference":"ref-1"},"visibility":"egress_only"}],"live_input_projection":true,"continuation_excluded_paths":["/mnt/memory"],"runtime_handle":{"kind":"host_bind_restoration","staging_root":"/provider/staging/a"},"sandbox_control_incarnation":{"kind":"kubernetes_pod","uid":"pod-uid-a"},"control_services":["repository_git_credential"]},"adoption_fingerprint":"fp-adoption","realization_fingerprint":"fp-realization","owned_paths":["/workspace/repo"]}}"#,
    ),
];

fn golden(name: &str) -> &'static str {
    HISTORICAL_WIRE
        .iter()
        .find_map(|(row, text)| (*row == name).then_some(*text))
        .unwrap()
}

#[test]
fn historical_wire_decodes_into_the_neutral_model_and_re_encodes_exactly() {
    // Decision table over every historical payload shape. C1 schema tag;
    // C2 neutral evidence members present; C3 restoration / Memory evidence.
    // E1 decode succeeds; E2 re-encode is byte-identical; E3 provider kind and
    // schema version come from the tag; E4 evidence is exposed iff C2, with the
    // exact recorded values; E5 Memory evidence is readable iff E4.
    let rows: &[(&str, &str, Option<u32>, bool)] = &[
        ("unmanaged", "test-tar", None, false),
        ("local_v1", "local", Some(1), false),
        ("local_v2", "local", Some(2), true),
        ("bubblewrap_v1", "bubblewrap", Some(1), false),
        ("bubblewrap_v2", "bubblewrap", Some(2), true),
        ("seatbelt_v1", "seatbelt", Some(1), false),
        ("seatbelt_v2", "seatbelt", Some(2), true),
        ("container_v1_docker", "container", Some(1), false),
        ("container_v1_k8s_legacy", "container", Some(1), false),
        ("container_v2_k8s", "container", Some(2), true),
        ("container_v2_k8s_no_claim", "container", Some(2), true),
        ("container_v2_host_bind", "container", Some(2), true),
    ];
    assert_eq!(rows.len(), HISTORICAL_WIRE.len(), "every golden has a rule");
    for (name, provider, version, current) in rows {
        let text = golden(name);
        let handle: SandboxHandle = serde_json::from_str(text).expect(name);
        assert_eq!(serde_json::to_string(&handle).unwrap(), text, "E2 {name}");
        assert_eq!(handle.provider_kind(), *provider, "E3 {name}");
        assert_eq!(
            handle.payload_schema().map(SandboxPayloadSchema::version),
            *version,
            "E3 {name}"
        );
        assert_eq!(handle.evidence().is_some(), *current, "E4 {name}");
        assert_eq!(
            handle.memory_materializations().unwrap().is_some(),
            *current,
            "E5 {name}"
        );
        let wire: serde_json::Value = serde_json::from_str(text).unwrap();
        if let Some(evidence) = handle.evidence() {
            assert_eq!(
                evidence.realization_fingerprint().as_str(),
                wire["payload"]["realization_fingerprint"],
                "E4 {name}"
            );
            assert_eq!(
                serde_json::to_value(evidence.owned_paths()).unwrap(),
                wire["payload"]["owned_paths"],
                "E4 {name}"
            );
            assert_eq!(
                evidence
                    .effect_fence()
                    .map(|fence| serde_json::to_value(fence).unwrap()),
                wire["payload"].get("effect_fence").cloned(),
                "E4 {name}"
            );
        }
        assert_eq!(
            handle.restoration().is_some(),
            wire.get("restoration").is_some(),
            "C3 {name}"
        );
    }
}

#[test]
fn member_order_is_encoding_evidence_not_identity() {
    // C1 the same historical handle arrives with every object's members in
    // lexical order (for example through a `serde_json::Value` without
    // `preserve_order`). E1 it decodes; E2 it equals the canonical decode;
    // E3 its provider members re-encode in arrival order (the schema tag is
    // always first), so re-encoding never silently rewrites a binding.
    for (name, text) in HISTORICAL_WIRE {
        let canonical: SandboxHandle = serde_json::from_str(text).unwrap();
        let wire: serde_json::Value = serde_json::from_str(text).unwrap();
        let lexical_text = lexical(&wire);
        let lexical_handle: SandboxHandle = serde_json::from_str(&lexical_text).expect(name);
        assert_eq!(lexical_handle, canonical, "E2 {name}");
        let mut payload = wire["payload"].clone();
        let tag = payload.as_object_mut().unwrap().remove("schema").unwrap();
        let expected_payload = format!("{{\"schema\":{},{}", tag, &lexical(&payload)[1..]);
        let reencoded = serde_json::to_string(&lexical_handle).unwrap();
        assert!(
            reencoded.contains(&expected_payload) || lexical(&payload) == "{}",
            "E3 {name}: {reencoded}"
        );
    }
}

/// Render JSON with every object's members sorted, independent of features.
fn lexical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut members: Vec<_> = map.iter().collect();
            members.sort_by(|left, right| left.0.cmp(right.0));
            let body: Vec<String> = members
                .into_iter()
                .map(|(name, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(name).unwrap(),
                        lexical(value)
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(values) => {
            let body: Vec<String> = values.iter().map(lexical).collect();
            format!("[{}]", body.join(","))
        }
        other => serde_json::to_string(other).unwrap(),
    }
}

#[test]
fn envelope_and_schema_grammar_fail_closed() {
    // Rejection table. Each malformed envelope or tag is refused at decode,
    // before any provider could interpret it.
    let payload =
        |payload: serde_json::Value| serde_json::json!({"sandbox_id": "s", "payload": payload});
    let rejected = [
        ("R1 missing schema", payload(serde_json::json!({"a": 1}))),
        (
            "R2 non-string schema",
            payload(serde_json::json!({"schema": 2})),
        ),
        (
            "R3 untagged version",
            payload(serde_json::json!({"schema": "local"})),
        ),
        (
            "R4 zero version",
            payload(serde_json::json!({"schema": "local_v0"})),
        ),
        (
            "R5 leading zero",
            payload(serde_json::json!({"schema": "local_v01"})),
        ),
        (
            "R6 empty provider",
            payload(serde_json::json!({"schema": "_v1"})),
        ),
        (
            "R7 uppercase provider",
            payload(serde_json::json!({"schema": "Local_v1"})),
        ),
        (
            "R8 foreign tag",
            payload(serde_json::json!({"schema": "opaque"})),
        ),
        (
            "R9 unmanaged extra member",
            payload(serde_json::json!({"schema": "unmanaged", "provider_kind": "k", "x": 1})),
        ),
        (
            "R10 unmanaged without kind",
            payload(serde_json::json!({"schema": "unmanaged"})),
        ),
        (
            "R11 unknown envelope field",
            serde_json::json!({"sandbox_id": "s", "payload": {"schema": "unmanaged", "provider_kind": "k"}, "extra": 1}),
        ),
        (
            "R12 fingerprint without owned paths",
            payload(serde_json::json!({"schema": "local_v2", "realization_fingerprint": "fp"})),
        ),
        (
            "R13 owned paths without fingerprint",
            payload(serde_json::json!({"schema": "container_v2", "owned_paths": []})),
        ),
        (
            "R14 fence without realization evidence",
            payload(serde_json::json!({"schema": "local_v2", "effect_fence": {}})),
        ),
        (
            "R15 malformed fence",
            payload(
                serde_json::json!({"schema": "local_v2", "realization_fingerprint": "fp", "owned_paths": [], "effect_fence": {"owner": "o"}}),
            ),
        ),
        (
            "R16 malformed owned paths",
            payload(
                serde_json::json!({"schema": "local_v2", "realization_fingerprint": "fp", "owned_paths": "/a"}),
            ),
        ),
        (
            "R17 non-object payload",
            payload(serde_json::json!(["local_v1"])),
        ),
    ];
    for (rule, wire) in rejected {
        assert!(
            serde_json::from_value::<SandboxHandle>(wire).is_err(),
            "{rule}"
        );
    }
    let accepted = serde_json::from_value::<SandboxHandle>(payload(
        serde_json::json!({"schema": "my_vm_provider_v12", "anything": {"nested": [1]}}),
    ))
    .expect("A1 an open provider name with a multi-digit version");
    assert_eq!(accepted.provider_kind(), "my_vm_provider");
    assert_eq!(accepted.payload_schema().unwrap().version(), 12);
    assert!(accepted.evidence().is_none(), "A1 legacy without evidence");
    assert!(SandboxPayloadSchema::new("ok", 0).is_err());
    assert!(SandboxPayloadSchema::new("9lives", 1).is_err());
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureV2 {
    locator: String,
    realization_fingerprint: crate::SandboxRealizationFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effect_fence: Option<SandboxEffectFence>,
    owned_paths: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FixtureV1 {
    locator: String,
}

fn fence(operation: &str, expires_at: u64) -> SandboxEffectFence {
    SandboxEffectFence::new(operation, "owner", "runtime", 1, expires_at).unwrap()
}

fn fixture(id: &str, payload: &FixtureV2) -> SandboxHandle {
    SandboxHandle::from_provider_payload(
        id,
        SandboxPayloadSchema::new("fixture", 2).unwrap(),
        payload,
    )
    .unwrap()
}

#[test]
fn substrate_identity_uses_only_neutral_evidence_and_the_opaque_locator() {
    // Cause/effect decision table for `owned_paths_are_monotonic_to`.
    // C1 both handles carry evidence; C2 sandbox id, schema, locator,
    // fingerprint and fence effect identity match; C3 owned paths only grow.
    // R1 C1+C2+C3 admits (including a renewed fence expiry and a superset);
    // R2 !C1 rejects (legacy/unmanaged never mint evidence); R3 each C2
    // difference rejects; R4 !C3 rejects.
    let fingerprint: crate::SandboxRealizationFingerprint =
        serde_json::from_value(serde_json::json!("fp")).unwrap();
    let base = FixtureV2 {
        locator: "physical-a".into(),
        realization_fingerprint: fingerprint.clone(),
        effect_fence: Some(fence("effect", 10)),
        owned_paths: vec!["/workspace/a".into()],
    };
    let current = fixture("s", &base);
    let with = |edit: &dyn Fn(&mut FixtureV2)| {
        let mut payload = base.clone();
        edit(&mut payload);
        fixture("s", &payload)
    };
    let admitted = [
        ("R1 replay", current.clone()),
        (
            "R1 superset",
            with(&|p| p.owned_paths.push("/workspace/b".into())),
        ),
        (
            "R1 renewed expiry",
            with(&|p| p.effect_fence = Some(fence("effect", 99))),
        ),
    ];
    for (rule, next) in admitted {
        assert!(current.owned_paths_are_monotonic_to(&next), "{rule}");
    }
    let legacy = SandboxHandle::from_provider_payload(
        "s",
        SandboxPayloadSchema::new("fixture", 1).unwrap(),
        &FixtureV1 {
            locator: "physical-a".into(),
        },
    )
    .unwrap();
    let rejected = [
        ("R2 legacy target", legacy.clone()),
        ("R2 unmanaged target", SandboxHandle::new("fixture", "s")),
        ("R3 sandbox id", fixture("other", &base)),
        (
            "R3 schema",
            SandboxHandle::from_provider_payload(
                "s",
                SandboxPayloadSchema::new("fixture", 3).unwrap(),
                &base,
            )
            .unwrap(),
        ),
        ("R3 locator", with(&|p| p.locator = "physical-b".into())),
        (
            "R3 fingerprint",
            with(&|p| {
                p.realization_fingerprint =
                    serde_json::from_value(serde_json::json!("other")).unwrap()
            }),
        ),
        (
            "R3 fence identity",
            with(&|p| p.effect_fence = Some(fence("other", 10))),
        ),
        ("R3 fence absence", with(&|p| p.effect_fence = None)),
        ("R4 path loss", with(&|p| p.owned_paths.clear())),
    ];
    for (rule, next) in rejected {
        assert!(!current.owned_paths_are_monotonic_to(&next), "{rule}");
    }
    assert!(
        !legacy.owned_paths_are_monotonic_to(&legacy),
        "R2 legacy source"
    );
    assert!(
        !SandboxHandle::new("fixture", "s")
            .owned_paths_are_monotonic_to(&SandboxHandle::new("fixture", "s")),
        "R2 unmanaged source"
    );
    assert!(
        legacy
            .clone()
            .with_memory_materializations(Vec::new())
            .is_err(),
        "legacy handles cannot be upgraded with Memory evidence"
    );
}

#[test]
fn provider_payload_codec_is_typed_and_fails_closed() {
    // P1 a typed payload round-trips through the opaque envelope; P2 a payload
    // may not define the reserved schema member; P3 a non-object payload is
    // refused; P4 a mismatched provider type is an invalid handle, not a
    // fallback; P5 an unmanaged handle has no payload to decode.
    let fingerprint: crate::SandboxRealizationFingerprint =
        serde_json::from_value(serde_json::json!("fp")).unwrap();
    let payload = FixtureV2 {
        locator: "physical-a".into(),
        realization_fingerprint: fingerprint,
        effect_fence: None,
        owned_paths: Vec::new(),
    };
    let handle = fixture("s", &payload);
    let decoded: SandboxHandle =
        serde_json::from_str(&serde_json::to_string(&handle).unwrap()).unwrap();
    assert_eq!(
        decoded.decode_provider_payload::<FixtureV2>().unwrap(),
        payload,
        "P1"
    );

    #[derive(Serialize)]
    struct Reserved {
        schema: &'static str,
    }
    assert!(
        SandboxHandle::from_provider_payload(
            "s",
            SandboxPayloadSchema::new("fixture", 1).unwrap(),
            &Reserved { schema: "x" }
        )
        .is_err(),
        "P2"
    );
    assert!(
        SandboxHandle::from_provider_payload(
            "s",
            SandboxPayloadSchema::new("fixture", 1).unwrap(),
            &1_u8
        )
        .is_err(),
        "P3"
    );

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Foreign {
        container_id: String,
    }
    assert!(
        matches!(
            decoded.decode_provider_payload::<Foreign>(),
            Err(SandboxError::Invalid(_))
        ),
        "P4"
    );
    assert!(
        matches!(
            SandboxHandle::new("fixture", "s").decode_provider_payload::<FixtureV2>(),
            Err(SandboxError::Invalid(_))
        ),
        "P5"
    );
    assert!(
        decoded
            .provider_mismatch("other")
            .to_string()
            .contains("other provider cannot adopt \"fixture\""),
    );
}
