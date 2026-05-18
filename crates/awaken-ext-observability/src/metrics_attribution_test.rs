use super::SpanContext;

#[test]
fn span_context_default_has_empty_attribution() {
    let ctx = SpanContext::default();
    assert!(ctx.prompt_id.is_none());
    assert!(ctx.tool_desc_ids.is_empty());
    assert!(ctx.skill_ids.is_empty());
    assert!(ctx.release_tag.is_none());
    assert!(ctx.experiment_id.is_none());
    assert!(ctx.variant_name.is_none());
}

#[test]
fn span_context_serializes_attribution_fields() {
    let ctx = SpanContext {
        prompt_id: Some("a1b2c3d4e5f6".to_string()),
        tool_desc_ids: vec!["t000aaaaaaaa".to_string(), "t111bbbbbbbb".to_string()],
        skill_ids: vec!["s00000000000".to_string()],
        release_tag: Some("agents.weather@stable".to_string()),
        experiment_id: Some("01HXEXP00000000000000000AB".to_string()),
        variant_name: Some("candidate".to_string()),
        ..Default::default()
    };

    let json = serde_json::to_value(&ctx).expect("serialise");
    assert_eq!(json["prompt_id"], "a1b2c3d4e5f6");
    assert_eq!(json["tool_desc_ids"][0], "t000aaaaaaaa");
    assert_eq!(json["skill_ids"][0], "s00000000000");
    assert_eq!(json["release_tag"], "agents.weather@stable");
    assert_eq!(json["experiment_id"], "01HXEXP00000000000000000AB");
    assert_eq!(json["variant_name"], "candidate");
}

#[test]
fn span_context_omits_empty_attribution_fields() {
    let ctx = SpanContext::default();
    let json = serde_json::to_string(&ctx).expect("serialise");
    // The "{}" invariant relies on every pre-existing field also using
    // skip_serializing_if (String::is_empty / Option::is_none). If a
    // future change drops a skip on an unrelated field, this assertion
    // will fail here even though the new attribution fields are fine —
    // chase the regression to that field, not to attribution.
    assert_eq!(json, "{}");
}
