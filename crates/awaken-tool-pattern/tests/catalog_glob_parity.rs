//! Catalog tool-id pattern parity oracle.
//!
//! Loads `tests/fixtures/catalog-glob-parity.json` and asserts the documented
//! behavior of [`awaken_tool_pattern::tool_id_match`] — the matcher used by
//! the catalog filter on `AgentSpec.allowed_tools` / `excluded_tools`. The
//! same fixture is consumed by the admin-console frontend matcher tests so
//! any divergence between runtime and UI is caught here.

use awaken_tool_pattern::tool_id_match;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    pattern: String,
    value: String,
    expected: bool,
    #[allow(dead_code)]
    note: String,
}

#[test]
fn catalog_tool_id_parity_oracle() {
    let raw = include_str!("fixtures/catalog-glob-parity.json");
    let cases: Vec<Case> = serde_json::from_str(raw).expect("parse fixture");

    let mut failures = Vec::new();
    for case in &cases {
        let got = tool_id_match(&case.pattern, &case.value);
        if got != case.expected {
            failures.push(format!(
                "  pattern={:?} value={:?} expected={} got={}  ({})",
                case.pattern, case.value, case.expected, got, case.note
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "catalog tool-id parity mismatches:\n{}",
        failures.join("\n")
    );
}
