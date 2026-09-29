//! Claude Managed Agents conformance for the managed agent config surface.
//!
//! `agent_config_from_managed` accepts a CMA-shaped agent body and
//! `managed_from_agent_config` projects one back, so a CMA client can drive this
//! repository with the official SDK. That only holds while the two agree on more
//! than field names: CMA documents validation rules whose rejection a client is
//! entitled to rely on, and a roster this surface accepts but CMA would refuse
//! is a body no CMA deployment would ever have produced.
//!
//! Each rule below is one documented CMA validation. The test does not assume
//! which are enforced - it submits the violating body and compares the outcome
//! against `contracts/cma-conformance-baseline.json`:
//!
//! - rejected, and absent from the baseline => conformant, guarded against
//!   regression.
//! - accepted, and listed in the baseline => a known, recorded divergence.
//! - accepted, and *not* listed => a new divergence; the test fails.
//! - rejected, but still listed => the rule was enforced and the baseline is
//!   stale; the test fails and names the entry to delete.
//!
//! The last two arms are what makes this a ratchet rather than a snapshot: the
//! list can only shrink, and a fix cannot land without recording that it landed.
//! Pinning today's acceptance as "expected" instead would turn a divergence into
//! a contract, which is the opposite of conformance.

use awaken_config_service::agent_config_from_managed;
use serde_json::{Value, json};

const BASELINE: &str = include_str!("../contracts/cma-conformance-baseline.json");

fn unenforced() -> Vec<String> {
    serde_json::from_str::<Value>(BASELINE)
        .expect("baseline parses")
        .get("unenforced")
        .and_then(Value::as_array)
        .expect("baseline states an unenforced list")
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .expect("every baseline entry is a rule name")
                .to_string()
        })
        .collect()
}

fn coordinator(agents: Value) -> Value {
    json!({
        "system": "coordinate specialists",
        "multiagent": { "type": "coordinator", "agents": agents }
    })
}

/// One documented CMA rule, the body that violates it, and why CMA refuses.
struct Rule {
    name: &'static str,
    /// What the platform reference states, so a reader can check the rule
    /// itself rather than trusting this file.
    cma_says: &'static str,
    body: Value,
}

fn rules() -> Vec<Rule> {
    let many: Vec<String> = (0..21).map(|index| format!("agent{index}")).collect();
    vec![
        Rule {
            name: "roster_entry_count_is_one_to_twenty",
            cma_says: "`agents` lists 1-20 roster entries.",
            body: coordinator(json!(many)),
        },
        Rule {
            name: "roster_holds_at_most_one_self",
            cma_says: "at most one `self` entry per roster.",
            body: coordinator(json!([{ "type": "self" }, { "type": "self" }])),
        },
        Rule {
            name: "roster_names_are_unique",
            cma_says: "Names must be unique across the roster.",
            body: coordinator(json!(["duplicate", "duplicate"])),
        },
        Rule {
            name: "roster_member_is_not_named_self",
            cma_says: "don't name an agent `self`.",
            body: coordinator(json!(["self"])),
        },
        Rule {
            name: "roster_holds_at_most_one_advisor",
            cma_says: "At most one advisor entry per roster.",
            body: coordinator(json!([
                { "type": "advisor", "model": "claude-opus-5" },
                { "type": "advisor", "model": "claude-opus-5" }
            ])),
        },
        Rule {
            name: "roster_member_does_not_take_the_reserved_advisor_name",
            cma_says: "a roster that also lists a member literally named \
                       `anthropic.advisor` is a 400.",
            body: coordinator(json!(["anthropic.advisor"])),
        },
    ]
}

/// Rules this file checks outside the roster table, so baseline validation can
/// still recognise their entries.
const NON_ROSTER_RULES: [&str; 1] = ["delegation_is_one_level_deep"];

/// A baseline entry naming no rule at all would sit there forever: the roster
/// table never reaches it, so neither the new-divergence nor the stale-entry arm
/// can fire, and a renamed or misspelled rule would look recorded while nothing
/// checks it. Every entry must therefore resolve to a rule this file knows.
#[test]
fn every_baseline_entry_names_a_rule_this_file_checks() {
    let known: Vec<String> = rules()
        .into_iter()
        .map(|rule| rule.name.to_string())
        .chain(NON_ROSTER_RULES.iter().map(|name| name.to_string()))
        .collect();
    let unknown: Vec<String> = unenforced()
        .into_iter()
        .filter(|entry| !known.contains(entry))
        .collect();
    assert!(
        unknown.is_empty(),
        "these baseline entries name no rule this file checks - a rule was renamed, \
         misspelled, or removed without shrinking the baseline:\n  {}",
        unknown.join("\n  ")
    );
}

#[test]
fn documented_cma_roster_rules_are_enforced_or_recorded_as_unenforced() {
    let baseline = unenforced();
    let mut newly_diverging = Vec::new();
    let mut stale_baseline = Vec::new();

    for rule in rules() {
        let accepted = agent_config_from_managed("conformance".into(), &rule.body).is_ok();
        let recorded = baseline.iter().any(|entry| entry == rule.name);
        match (accepted, recorded) {
            // A new body CMA would refuse and this surface takes: the gap the
            // ratchet exists to stop growing.
            (true, false) => {
                newly_diverging.push(format!("{} - CMA: {}", rule.name, rule.cma_says))
            }
            // The rule now rejects, so the baseline must lose its entry or the
            // next reader will believe a solved problem is still open.
            (false, true) => stale_baseline.push(rule.name),
            _ => {}
        }
    }

    assert!(
        newly_diverging.is_empty(),
        "these managed agent bodies violate a documented CMA rule and were accepted; \
         enforce the rule or record it in contracts/cma-conformance-baseline.json:\n  {}",
        newly_diverging.join("\n  ")
    );
    assert!(
        stale_baseline.is_empty(),
        "these rules are enforced now - delete them from \
         contracts/cma-conformance-baseline.json so the ratchet shrinks:\n  {}",
        stale_baseline.join("\n  ")
    );
}

/// CMA allows exactly one level of delegation and rejects a roster whose member
/// carries its own roster - "enforced rather than silently flattened". This
/// repository instead carries a `DelegationLimits` budget whose default permits
/// eight levels, so a body that states no limit at all still resolves to a depth
/// a CMA client is entitled to assume impossible. The depth is the value a
/// client would reason about, so the assertion names it rather than only
/// checking that the body parsed.
#[test]
fn delegation_depth_default_is_recorded_against_the_one_level_cma_contract() {
    let config = agent_config_from_managed("conformance".into(), &coordinator(json!(["worker"])))
        .expect("a coordinator body with no stated limits parses");
    let depth = config.delegation_limits.max_depth;

    if unenforced()
        .iter()
        .any(|entry| entry == "delegation_is_one_level_deep")
    {
        assert!(
            depth > 1,
            "delegation_is_one_level_deep is still listed as unenforced, but the \
             default depth is {depth}; delete the entry from \
             contracts/cma-conformance-baseline.json"
        );
    } else {
        assert_eq!(
            depth, 1,
            "CMA permits one level of delegation, so a managed body that states no \
             limit must resolve to depth 1"
        );
    }
}
