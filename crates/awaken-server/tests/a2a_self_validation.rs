#[test]
fn a2a_text_part_concat_logic() {
    let parts = serde_json::json!([
        {"type":"text","text":"hello "},
        {"type":"image","url":"u"},
        {"type":"text","text":"world"}
    ]);
    let text = parts
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "hello world");
}
