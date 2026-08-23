use std::collections::BTreeMap;

use super::{
    AcpSpec, Backend, BackendModelSelection, ContextPolicy, InferencePlacementMechanism,
    ModelBinding, ResolvedModelCandidate, ResolvedSpec, ToolDescriptor, ToolFacet, ToolKind,
    ToolPresentation, content_hash, normalize_model_tool_schema,
};

#[test]
fn placement_mechanism_has_one_stable_boundary_vocabulary() {
    // Cause/effect decision table: each supported mechanism must round-trip
    // through its stable cross-context string (R1/R2); an unknown value must
    // fail instead of selecting a default mechanism (R3).
    for (rule, mechanism, wire) in [
        (
            "R1",
            InferencePlacementMechanism::AnthropicRequestBody,
            "anthropic_request_body",
        ),
        (
            "R2",
            InferencePlacementMechanism::FrozenRegionalRoute,
            "frozen_regional_route",
        ),
    ] {
        assert_eq!(mechanism.as_str(), wire, "{rule}");
        assert_eq!(wire.parse(), Ok(mechanism), "{rule}");
    }
    assert!(
        "caller_selected"
            .parse::<InferencePlacementMechanism>()
            .is_err(),
        "R3"
    );
}

fn td(id: &str) -> ToolDescriptor {
    ToolDescriptor::pinned("t", id, format!("desc of {id}"), serde_json::json!({}))
}

#[test]
fn empty_presentation_is_the_identity() {
    let p = ToolPresentation::default();
    assert!(p.is_empty());
    let tools = vec![td("a"), td("mcp__x__y")];
    let out = p.present(&tools);
    assert_eq!(out.face, tools, "no overrides ⇒ face unchanged");
    assert!(out.deferred.is_empty());
    assert_eq!(p.resolve("a"), "a", "no alias ⇒ resolve is identity");
}

#[test]
fn present_renames_redescribes_and_defers_by_canonical_id() {
    // Works identically for a static id and an MCP id.
    let p = ToolPresentation::from_facets([
        (
            "a".to_string(),
            ToolFacet {
                alias: Some("say".into()),
                description: Some("Speak.".into()),
                defer: false,
            },
        ),
        (
            "mcp__x__y".to_string(),
            ToolFacet {
                alias: Some("y".into()),
                description: None,
                defer: true,
            },
        ),
        ("noop".to_string(), ToolFacet::default()), // all-default ⇒ dropped
    ]);
    assert!(!p.is_empty());
    let out = p.present(&[td("a"), td("mcp__x__y"), td("keep")]);
    // `a` renamed + redescribed and stays in the face; `keep` passes through.
    assert!(
        out.face
            .iter()
            .any(|d| d.id == "say" && d.description == "Speak.")
    );
    assert!(out.face.iter().any(|d| d.id == "keep"));
    // The MCP tool is deferred (renamed) — withheld from the face.
    assert!(out.face.iter().all(|d| d.id != "y"));
    assert!(out.deferred.iter().any(|d| d.id == "y"));
}

#[test]
fn model_tools_withholds_a_deferred_tool_until_opened_and_offers_tool_open() {
    use super::TOOL_OPEN_ID;
    let p = ToolPresentation::from_facets([(
        "mcp__srv__a".to_string(),
        ToolFacet {
            alias: Some("create_issue".into()),
            description: None,
            defer: true,
        },
    )]);
    let tools = [td("mcp__srv__a"), td("keep")];

    // Nothing opened: the deferred tool is withheld; tool_open is offered.
    let closed = std::collections::BTreeSet::new();
    let face = p.model_tools(&tools, &closed);
    let ids: Vec<&str> = face.iter().map(|d| d.id.as_str()).collect();
    assert!(ids.contains(&"keep"));
    assert!(ids.contains(&TOOL_OPEN_ID));
    assert!(!ids.contains(&"create_issue"), "deferred tool withheld");

    // Opened (by canonical id): the tool appears, and tool_open is gone.
    let opened: std::collections::BTreeSet<String> = ["mcp__srv__a".to_string()].into();
    let ids2: Vec<String> = p
        .model_tools(&tools, &opened)
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert!(ids2.contains(&"create_issue".to_string()));
    assert!(
        !ids2.iter().any(|i| i == TOOL_OPEN_ID),
        "no deferred left ⇒ no tool_open"
    );
}

#[test]
fn resolve_reverses_an_alias_to_its_canonical_id() {
    let p = ToolPresentation::from_facets([(
        "mcp__x__y".to_string(),
        ToolFacet {
            alias: Some("y".into()),
            ..Default::default()
        },
    )]);
    // The single choke: a model call by alias reverses to the canonical id; a
    // non-alias (e.g. an un-renamed tool) passes through untouched.
    assert_eq!(p.resolve("y"), "mcp__x__y");
    assert_eq!(p.resolve("other"), "other");
}

// Cause/effect decision table for the sole ACP plugin-config codec:
// R1 known values + unknown ACP/non-ACP keys -> decode/encode byte value stable;
// R2 clearing an owned value -> remove only that key;
// R3 no ACP keys remain -> remove the empty ACP section;
// R4 unrecognized historical MCP wire -> preserve it byte-for-byte.
#[test]
fn acp_spec_round_trip_preserves_unowned_plugin_configuration() {
    let original = BTreeMap::from([
        ("other".into(), serde_json::json!({"enabled": true})),
        (
            "acp".into(),
            serde_json::json!({
                "compact_window": 120_000,
                "mcp_servers": [{
                    "name": "github",
                    "transport": {"kind": "http", "url": "https://mcp.invalid"}
                }],
                "adapter_extension": {"native": 7}
            }),
        ),
    ]);
    let spec = AcpSpec::from_plugin_config(&original);
    assert_eq!(
        spec.clone().into_plugin_config(original.clone()),
        original,
        "R1"
    );

    let without_window = AcpSpec {
        compact_window: None,
        ..spec
    }
    .into_plugin_config(original);
    assert!(without_window["acp"].get("compact_window").is_none(), "R2");
    assert_eq!(
        without_window["acp"]["adapter_extension"],
        serde_json::json!({"native": 7}),
        "R2"
    );

    assert!(
        !AcpSpec::default()
            .into_plugin_config(BTreeMap::from([(
                "acp".into(),
                serde_json::json!({"compact_window": 1})
            )]))
            .contains_key("acp"),
        "R3"
    );

    let historical = BTreeMap::from([(
        "acp".into(),
        serde_json::json!({
            "compact_window": 7,
            "mcp_servers": [{"name": "legacy", "url": "https://mcp.invalid"}]
        }),
    )]);
    assert_eq!(
        AcpSpec::from_plugin_config(&historical).into_plugin_config(historical.clone()),
        historical,
        "R4"
    );
}

#[test]
fn backend_typed_view_distinguishes_native_and_acp() {
    // Any non-`acp` ref is Native — including the provider axis "genai", which
    // Backend must NOT absorb as a fourth kind.
    assert_eq!(Backend::from_ref("genai"), Backend::Native);
    assert_eq!(Backend::from_ref("default"), Backend::Native);
    assert!(!Backend::from_ref("genai").is_acp());

    // Only exact `acp:<profile>` routes are runnable; a bare family token is
    // authoring syntax and cannot become an execution backend.
    assert!(matches!(Backend::from_ref("acp"), Backend::Invalid(_)));
    assert!(matches!(Backend::from_ref("acp:"), Backend::Invalid(_)));
    assert!(
        matches!(Backend::from_ref("acp:claude"), Backend::Acp(cli) if cli.cli() == "claude" && cli.backend_ref() == "acp:claude")
    );
    assert!(Backend::from_ref("acp:codex").is_acp());

    // The binding's stored `backend_ref` parses to the same typed backend.
    assert!(matches!(
        Backend::from_ref(&ModelBinding::new("p", "m", "acp:codex").backend_ref),
        Backend::Acp(cli) if cli.cli() == "codex"
    ));

    // `a2a:<endpoint>` is a remote A2A backend.
    assert!(matches!(
        Backend::from_ref("a2a:https://host/a2a"),
        Backend::Remote(endpoint) if endpoint.endpoint() == "https://host/a2a"
    ));
    assert_eq!(
        Backend::from_ref("a2a:https://host/a2a").remote_endpoint(),
        Some("https://host/a2a")
    );
    assert!(matches!(Backend::from_ref("a2a:x"), Backend::Invalid(_)));
    assert!(matches!(Backend::from_ref("a2a:"), Backend::Invalid(_)));
    assert!(matches!(
        Backend::from_ref("a2a:ftp://agent.example"),
        Backend::Invalid(_)
    ));
    assert!(matches!(
        Backend::from_ref(" acp:codex"),
        Backend::Invalid(_)
    ));
    assert!(matches!(Backend::from_ref(""), Backend::Invalid(_)));
}

#[test]
fn backend_owned_candidate_serializes_only_identity_and_model_policy() {
    // Cause graph: exact Worker-local reference + explicit model policy ->
    // immutable BackendOwned candidate. Endpoint and material have no fields.
    //
    // Decision table: Default and Exact both round-trip; changing the policy
    // changes the snapshot data without inventing a model-id sentinel.
    for selection in [BackendModelSelection::Default, BackendModelSelection::Exact] {
        let model = if selection == BackendModelSelection::Default {
            ""
        } else {
            "gpt-exact"
        };
        let candidate = ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("cred:local", model, "acp:codex"),
            crate::CredentialRef {
                id: "cred:local".into(),
                revision: 3,
            },
            selection,
            "test",
            "sha256:test-capability",
            Default::default(),
        )
        .expect("coherent backend-owned candidate");
        let wire = serde_json::to_string(&candidate).unwrap();
        assert!(!wire.contains("base_url"));
        assert!(!wire.contains("material"));
        assert_eq!(
            serde_json::from_str::<ResolvedModelCandidate>(&wire).unwrap(),
            candidate
        );
    }
}

#[test]
fn execution_model_selection_is_limited_to_the_published_pool() {
    let mut spec = crate::snapshot::ExecutableAgentSnapshot::builder("agent")
        .model(ModelBinding::new("primary-id", "primary", "native"))
        .model_candidates([ModelBinding::new("fallback-id", "fallback", "native")])
        .build()
        .resolved_spec;
    let unchanged = spec.clone();

    assert!(!spec.select_execution_model("not-published"));
    assert_eq!(
        spec, unchanged,
        "a rejected selector cannot mutate the pool"
    );

    assert!(spec.select_execution_model("fallback"));
    assert_eq!(spec.model_binding.provider_identity_ref, "fallback-id");
    assert!(spec.model_candidates.is_empty());
}

#[test]
fn attempt_candidates_add_advisor_without_turning_it_into_a_fallback() {
    // Cause/effect graph: the primary pool controls model failover, while a
    // distinct advisor still needs attempt-fenced credentials and routing.
    // An advisor identical to the primary is one route, not two claims.
    //
    // Decision table:
    // | Rule | pool       | advisor  | attempt set | failover set |
    // | C1   | primary+fb | absent   | 2           | 2            |
    // | C2   | primary+fb | distinct | 3           | 2            |
    // | C3   | primary+fb | primary  | 2           | 2            |
    // | C4   | no match   | distinct | 0           | 0            |
    let mut spec = crate::snapshot::ExecutableAgentSnapshot::builder("agent")
        .model(ModelBinding::new("primary-id", "primary", "native"))
        .model_candidates([ModelBinding::new("fallback-id", "fallback", "native")])
        .build()
        .resolved_spec;
    assert_eq!(spec.attempt_candidates(None).len(), 2, "C1");

    let advisor =
        ResolvedModelCandidate::host(ModelBinding::new("advisor-id", "advisor", "native"));
    spec.plugin_config.agent.advisor = Some(crate::agent_bindings::AgentAdvisorBinding {
        model: "claude-opus-5".into(),
        candidate: advisor.clone(),
    });
    assert_eq!(spec.attempt_candidates(None).len(), 3, "C2");
    assert_eq!(spec.candidate_bindings().len(), 2, "C2");
    assert_eq!(spec.candidate_for_binding(&advisor.binding), Some(&advisor));

    spec.plugin_config.agent.advisor = Some(crate::agent_bindings::AgentAdvisorBinding {
        model: "claude-primary".into(),
        candidate: spec.model_binding.clone(),
    });
    assert_eq!(spec.attempt_candidates(None).len(), 2, "C3");
    assert_eq!(spec.candidate_bindings().len(), 2, "C3");
    assert!(
        spec.attempt_candidates(Some("not-published")).is_empty(),
        "C4"
    );
}

#[test]
fn a_legacy_spec_without_the_defaulted_fields_still_loads() {
    // The `#[serde(default)]` fields (model_candidates, plugin_config,
    // context_policy, tool_presentation) exist so a snapshot compiled before they
    // were added stays loadable. A JSON carrying only the required surface must
    // deserialize with each optional field at its documented default — the very
    // backward-compat promise those attributes make.
    let legacy = serde_json::json!({
        "catalog_fingerprint": "fp-1",
        "instructions": "be concise",
        "max_steps": 8,
        "model_binding": {
            "provider_identity_ref": "p",
            "model_ref": "m",
            "backend_ref": "genai"
        },
        "tool_descriptors": [],
        "plugin_ids": []
    });
    let spec: ResolvedSpec = serde_json::from_value(legacy).expect("legacy spec loads");
    assert!(spec.model_candidates.is_empty());
    assert!(spec.plugin_config.is_empty());
    assert!(spec.tool_presentation.is_empty());
    // An unset context policy sends the whole transcript (KeepAll), unchanged behavior.
    assert_eq!(spec.context_policy, ContextPolicy::KeepAll);
    // A single-model agent yields exactly the primary binding.
    assert_eq!(spec.candidate_bindings().len(), 1);
}

#[test]
fn context_policy_wire_is_internally_tagged_snake_case_and_defaults_to_keep_all() {
    // `tag = "kind"`, `rename_all = "snake_case"`: the unit variant carries just its
    // tag, the struct variant its fields alongside.
    assert_eq!(
        serde_json::to_value(ContextPolicy::KeepAll).unwrap(),
        serde_json::json!({ "kind": "keep_all" })
    );
    assert_eq!(
        serde_json::to_value(ContextPolicy::KeepLast { keep_last: 3 }).unwrap(),
        serde_json::json!({ "kind": "keep_last", "keep_last": 3 })
    );
    // Both directions round-trip, and the derived default is KeepAll.
    let back: ContextPolicy =
        serde_json::from_value(serde_json::json!({ "kind": "keep_last", "keep_last": 0 })).unwrap();
    assert_eq!(back, ContextPolicy::KeepLast { keep_last: 0 });
    assert_eq!(ContextPolicy::default(), ContextPolicy::KeepAll);
}

#[test]
fn content_hash_covers_id_description_and_schema() {
    let base = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 1}));

    // Same inputs hash equally.
    let same = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 1}));
    assert_eq!(base.content_hash(), same.content_hash());

    // Any surface change moves the hash.
    let schema_changed = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 2}));
    let desc_changed = ToolDescriptor::pinned("p", "t", "other", serde_json::json!({"a": 1}));
    let id_changed = ToolDescriptor::pinned("p", "u", "desc", serde_json::json!({"a": 1}));
    assert_ne!(base.content_hash(), schema_changed.content_hash());
    assert_ne!(base.content_hash(), desc_changed.content_hash());
    assert_ne!(base.content_hash(), id_changed.content_hash());
    assert!(base.content_hash().starts_with("p:t:"));
}

#[test]
fn descriptor_state_owns_one_derived_content_identity() {
    // Metamorphic contract: every semantic mutation changes the derived
    // identity; serialization carries the source facts, never a stale hash
    // that could disagree with them.
    let base = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({}));
    let kind_changed = base.clone().with_kind(ToolKind::Advisor);
    let recovery_changed = base
        .clone()
        .with_recovery(crate::tool::ToolRecoveryPolicy::durable_request());
    assert_ne!(base.content_hash(), kind_changed.content_hash());
    assert_ne!(base.content_hash(), recovery_changed.content_hash());

    let encoded = serde_json::to_value(&base).expect("serialize descriptor facts");
    assert!(encoded.get("content_hash").is_none());
    let decoded: ToolDescriptor =
        serde_json::from_value(encoded).expect("deserialize valid descriptor facts");
    assert_eq!(decoded.content_hash(), base.content_hash());
}

#[test]
fn persisted_descriptor_namespace_has_one_versioned_decode_boundary() {
    // Cause/effect graph: C1=the current namespace fact is present;
    // C2=only the former derived content_hash is present; C3=that legacy hash
    // binds the exact descriptor id and a 16-hex digest; C4=an old semantic
    // kind/recovery suffix follows the digest. E1=construct the descriptor from
    // source facts; E2=recover the one namespace; E3=fail closed.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | R1   | Y  | -  | -  | -  | E1     |
    // | R2   | N  | Y  | Y  | N  | E1+E2  |
    // | R3   | N  | Y  | Y  | Y  | E1+E2  |
    // | R4   | N  | N  | -  | -  | E3     |
    // | R5   | N  | Y  | N  | -  | E3     |
    let current = ToolDescriptor::pinned("fixture", "fixture_tool", "run", serde_json::json!({}));
    let current_wire = serde_json::to_value(&current).expect("R1 serialize current facts");
    let current_round_trip: ToolDescriptor =
        serde_json::from_value(current_wire).expect("R1 decode current facts");
    assert_eq!(current_round_trip.content_hash(), current.content_hash());

    for legacy_hash in [
        "fixture:fixture_tool:0123456789abcdef",
        "fixture:fixture_tool:0123456789abcdef:kind:Advisor:recovery:{}",
    ] {
        let decoded: ToolDescriptor = serde_json::from_value(serde_json::json!({
            "content_hash": legacy_hash,
            "id": "fixture_tool",
            "description": "run",
            "parameters": {}
        }))
        .expect("R2/R3 decode persisted descriptor facts");
        assert!(decoded.content_hash().starts_with("fixture:fixture_tool:"));
    }

    for invalid in [
        serde_json::json!({
            "id": "fixture_tool",
            "description": "run",
            "parameters": {}
        }),
        serde_json::json!({
            "content_hash": "fixture:other:0123456789abcdef",
            "id": "fixture_tool",
            "description": "run",
            "parameters": {}
        }),
    ] {
        assert!(serde_json::from_value::<ToolDescriptor>(invalid).is_err());
    }
}

#[test]
fn persisted_invalid_tool_schema_never_constructs_a_descriptor() {
    let descriptor = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({}));
    let mut encoded = serde_json::to_value(descriptor).expect("serialize descriptor");
    encoded["parameters"] = serde_json::json!({"type": "string"});
    assert!(serde_json::from_value::<ToolDescriptor>(encoded).is_err());
}

#[test]
fn model_tool_schema_normalization_decision_table() {
    // Cause graph: C1=root is an object, C2=root type is object,
    // C3=properties is missing/object/invalid, C4=nested array lacks items.
    // Effects: E1=canonical schema, E2=preserve valid fields,
    // E3=reject before provider I/O.
    //
    // | Rule | C1 | C2 | C3      | C4 | Effect |
    // | R1   | Y  | Y  | missing | -  | E1     |
    // | R2   | Y  | -  | missing | -  | E1     |
    // | R3   | Y  | Y  | object  | Y  | E1+E2  |
    // | R4   | Y  | Y  | invalid | -  | E3     |
    // | R5   | N  | -  | -       | -  | E3     |
    // | R6   | Y  | N  | -       | -  | E3     |
    let missing_properties =
        normalize_model_tool_schema(serde_json::json!({"type":"object"})).expect("R1");
    assert_eq!(missing_properties["properties"], serde_json::json!({}));

    let empty = normalize_model_tool_schema(serde_json::json!({})).expect("R2");
    assert_eq!(empty["type"], "object");
    assert_eq!(empty["properties"], serde_json::json!({}));

    let nested = normalize_model_tool_schema(serde_json::json!({
        "type":"object",
        "properties": {
            "filters": {
                "type":"object",
                "properties": {
                    "literal": {
                        "const": {"type":"object"}
                    }
                }
            },
            "names": {"type":"array"}
        },
        "additionalProperties": false
    }))
    .expect("R3");
    assert!(nested["properties"]["filters"]["properties"].is_object());
    assert_eq!(
        nested["properties"]["names"]["items"],
        serde_json::json!({})
    );
    assert_eq!(
        nested["properties"]["filters"]["properties"]["literal"]["const"],
        serde_json::json!({"type":"object"}),
        "R3 preserves instance-valued schema metadata"
    );
    assert_eq!(nested["additionalProperties"], false);

    assert!(
        normalize_model_tool_schema(serde_json::json!({"type":"object","properties":[]})).is_err(),
        "R4"
    );
    assert!(
        normalize_model_tool_schema(serde_json::json!(null)).is_err(),
        "R5"
    );
    assert!(
        normalize_model_tool_schema(serde_json::json!({"type":"string"})).is_err(),
        "R6"
    );
}

#[test]
fn pinned_descriptor_hashes_the_canonical_provider_schema() {
    // R1: a legacy-compatible empty object and an explicit zero-argument
    // object describe the same provider surface, so they must converge to
    // one schema and one content identity.
    let omitted = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({}));
    let explicit = ToolDescriptor::pinned(
        "p",
        "t",
        "desc",
        serde_json::json!({"type":"object","properties":{}}),
    );
    assert_eq!(omitted.parameters, explicit.parameters);
    assert_eq!(omitted.content_hash(), explicit.content_hash());
}

#[test]
fn content_hash_is_length_prefixed_against_field_concatenation_collisions() {
    // Without length-prefixing, ("ab","c") and ("a","bc") would concatenate to
    // the same byte stream and collide. The id is part of the readable prefix,
    // so vary the description/schema boundary where the digest actually matters.
    let recovery = crate::tool::ToolRecoveryPolicy::default();
    let a = content_hash(
        "p",
        "t",
        "ab",
        &serde_json::json!("c"),
        ToolKind::Regular,
        &recovery,
        None,
    );
    let b = content_hash(
        "p",
        "t",
        "a",
        &serde_json::json!("bc"),
        ToolKind::Regular,
        &recovery,
        None,
    );
    assert_ne!(a, b);
}

#[test]
fn content_hash_is_deterministic_sha256_hex() {
    // Portable digest: the same inputs always yield the same 16 hex chars, and
    // the tail is valid lowercase hex (not a platform-dependent SipHash value).
    let h = ToolDescriptor::pinned("p", "t", "desc", serde_json::json!({"a": 1})).content_hash();
    let tail = h.rsplit(':').next().unwrap();
    assert_eq!(tail.len(), 16);
    assert!(
        tail.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}
