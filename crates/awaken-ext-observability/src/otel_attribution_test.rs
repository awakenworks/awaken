use super::*;

fn span_with_attribution() -> GenAISpan {
    let mut ctx = SpanContext::default();
    ctx.run_id = "r".into();
    ctx.thread_id = "t".into();
    ctx.agent_id = "a".into();
    ctx.prompt_id = Some("a1b2c3d4e5f6".into());
    ctx.tool_desc_ids = vec!["t000aaaaaaaa".into()];
    ctx.skill_ids = vec!["s00000000000".into()];
    ctx.release_tag = Some("agents.weather@stable".into());
    ctx.experiment_id = Some("01HXEXP00000000000000000AB".into());
    ctx.variant_name = Some("candidate".into());
    GenAISpan {
        context: ctx,
        step_index: Some(0),
        model: "m".into(),
        provider: "p".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: vec![],
        error_type: None,
        error_class: None,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        thinking_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: vec![],
        duration_ms: 0,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    }
}

#[test]
fn genai_span_emits_awaken_attributes() {
    let attrs = otel_attributes_for_inference(&span_with_attribution());
    let map: std::collections::HashMap<_, _> = attrs
        .iter()
        .map(|kv| (kv.key.as_str().to_string(), kv.value.to_string()))
        .collect();
    assert_eq!(
        map.get("awaken.prompt_id").map(String::as_str),
        Some("a1b2c3d4e5f6")
    );
    assert_eq!(
        map.get("awaken.experiment.id").map(String::as_str),
        Some("01HXEXP00000000000000000AB")
    );
    assert_eq!(
        map.get("awaken.experiment.variant").map(String::as_str),
        Some("candidate")
    );
    assert_eq!(
        map.get("awaken.release.tag").map(String::as_str),
        Some("agents.weather@stable")
    );
    assert!(map.contains_key("awaken.tool_desc_ids"));
    assert!(map.contains_key("awaken.skill_ids"));
}

#[test]
fn attribution_skips_empty_fields() {
    let span = GenAISpan {
        context: SpanContext::default(),
        step_index: None,
        model: "m".into(),
        provider: "p".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: vec![],
        error_type: None,
        error_class: None,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        thinking_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: vec![],
        duration_ms: 0,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    };
    let attrs = otel_attributes_for_inference(&span);
    assert!(
        attrs.is_empty(),
        "no attribution attrs should be emitted for a default context"
    );
}

#[test]
fn tool_span_emits_awaken_attributes() {
    let mut ctx = SpanContext::default();
    ctx.prompt_id = Some("deadbeefcafe".into());
    ctx.experiment_id = Some("01HXEXP00000000000000000CD".into());
    ctx.variant_name = Some("control".into());
    let span = ToolSpan {
        context: ctx,
        step_index: None,
        name: "search".into(),
        operation: "execute_tool".into(),
        call_id: "call_1".into(),
        tool_type: "function".into(),
        call_arguments: None,
        call_result: None,
        error_type: None,
        duration_ms: 10,
        started_at_ms: 0,
        ended_at_ms: 0,
    };
    let attrs = otel_attributes_for_tool(&span);
    let map: std::collections::HashMap<_, _> = attrs
        .iter()
        .map(|kv| (kv.key.as_str().to_string(), kv.value.to_string()))
        .collect();
    assert_eq!(
        map.get("awaken.prompt_id").map(String::as_str),
        Some("deadbeefcafe")
    );
    assert_eq!(
        map.get("awaken.experiment.id").map(String::as_str),
        Some("01HXEXP00000000000000000CD")
    );
    assert_eq!(
        map.get("awaken.experiment.variant").map(String::as_str),
        Some("control")
    );
}
