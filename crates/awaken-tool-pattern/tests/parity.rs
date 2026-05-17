use awaken_tool_pattern::tool_id_match;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    pattern: String,
    value: String,
    expected: bool,
    #[serde(default)]
    note: String,
}

#[test]
fn parity_fixture_matches_runtime() {
    let raw = include_str!("fixtures/catalog-glob-parity.json");
    let cases: Vec<Case> = serde_json::from_str(raw).expect("fixture parses");
    assert!(!cases.is_empty(), "fixture must have cases");
    let mut failed = Vec::new();
    for c in &cases {
        let got = tool_id_match(&c.pattern, &c.value);
        if got != c.expected {
            failed.push(format!(
                "pattern={:?} value={:?} expected={} got={} note={:?}",
                c.pattern, c.value, c.expected, got, c.note
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "parity failures:\n  {}",
        failed.join("\n  ")
    );
}
