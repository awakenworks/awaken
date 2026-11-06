//! Claude Managed Agents roster conformance, checked against its owner.
//!
//! `MultiagentConfig::validate` is the single owner of the roster's
//! representation-independent invariants: managed admission
//! (`awaken-control/src/managed_agents.rs`), publication compilation
//! (`awaken-agent-config/src/compile/agent_bindings.rs`) and the managed
//! projection all call it, so a body that reaches a session has passed it. This
//! file pins each rule CMA documents against that owner, and records the two it
//! does not decide.
//!
//! Two earlier readings of this surface were wrong, and the shape of this file
//! is what they cost:
//!
//! - Testing `agent_config_from_managed` instead. That function *parses* a
//!   managed body; admission is elsewhere. A parser accepts a roster the
//!   validator later refuses, which looks like a missing rule and is not one.
//!   Authoring under `/v1/config/agents` is deliberately permissive - the
//!   `/validate` route is the author's dry run, and publication compilation is
//!   the gate - so probing the parser measured a design decision, not a gap.
//! - Reading `delegation_limits` on that surface as a CMA field. It is Awaken's
//!   own config plane, whose vocabulary is intentionally richer than CMA's.
//!
//! So the rules below are asserted where they are decided, and nowhere else.

use awaken_agent_config::{MultiagentConfig, MultiagentTarget};

const OWNER: &str = "coordinator";

fn agent(id: &str) -> MultiagentTarget {
    MultiagentTarget::Agent {
        id: id.to_string(),
        version: None,
    }
}

fn roster(agents: Vec<MultiagentTarget>) -> MultiagentConfig {
    MultiagentConfig { agents }
}

/// One rule CMA documents, a roster that violates it, and the text the platform
/// reference uses - quoted so a reader checks the rule, not this file's summary.
struct Rule {
    name: &'static str,
    cma_says: &'static str,
    violating: MultiagentConfig,
}

fn rules() -> Vec<Rule> {
    vec![
        Rule {
            name: "roster_states_at_least_one_entry",
            cma_says: "`agents` lists 1-20 roster entries.",
            violating: roster(vec![]),
        },
        Rule {
            name: "roster_holds_at_most_twenty_entries",
            cma_says: "`agents` lists 1-20 roster entries.",
            violating: roster((0..21).map(|i| agent(&format!("worker{i}"))).collect()),
        },
        Rule {
            name: "roster_holds_at_most_one_self",
            cma_says: "at most one `self` entry per roster.",
            violating: roster(vec![
                MultiagentTarget::SelfReference,
                MultiagentTarget::SelfReference,
            ]),
        },
        Rule {
            name: "roster_names_are_unique",
            cma_says: "Names must be unique across the roster.",
            violating: roster(vec![agent("duplicate"), agent("duplicate")]),
        },
        Rule {
            name: "recursion_uses_the_self_entry",
            cma_says: "don't name an agent `self` - the coordinator is reached \
                       through the `self` roster entry.",
            violating: roster(vec![agent(OWNER)]),
        },
        Rule {
            name: "roster_holds_at_most_one_advisor",
            cma_says: "At most one advisor entry per roster.",
            violating: roster(vec![
                MultiagentTarget::Advisor {
                    model: "claude-opus-5".into(),
                },
                MultiagentTarget::Advisor {
                    model: "claude-opus-5".into(),
                },
            ]),
        },
        Rule {
            name: "advisor_states_a_model",
            cma_says: "The entry has exactly two fields - `type` and `model`.",
            violating: roster(vec![MultiagentTarget::Advisor { model: "  ".into() }]),
        },
        Rule {
            name: "roster_member_does_not_take_the_reserved_advisor_name",
            cma_says: "a roster that also lists a member literally named \
                       `anthropic.advisor` is a 400.",
            violating: roster(vec![agent("anthropic.advisor")]),
        },
        Rule {
            name: "pinned_version_is_at_least_one",
            cma_says: "Omit `version` to pin the latest at coordinator save time.",
            violating: roster(vec![MultiagentTarget::Agent {
                id: "worker".into(),
                version: Some(0),
            }]),
        },
    ]
}

#[test]
fn documented_cma_roster_rules_are_refused_by_their_owner() {
    let mut admitted = Vec::new();
    for rule in rules() {
        if rule.violating.validate(OWNER).is_ok() {
            admitted.push(format!("{} - CMA: {}", rule.name, rule.cma_says));
        }
    }
    assert!(
        admitted.is_empty(),
        "these rosters violate a documented CMA rule and MultiagentConfig::validate \
         admitted them:\n  {}",
        admitted.join("\n  ")
    );
}

/// The complement of the table above: a roster CMA accepts must not be refused,
/// or the rules would pass by refusing everything.
#[test]
fn a_roster_cma_accepts_is_admitted() {
    let accepted = roster(vec![
        agent("researcher"),
        MultiagentTarget::Agent {
            id: "coder".into(),
            version: Some(7),
        },
        MultiagentTarget::SelfReference,
        MultiagentTarget::Advisor {
            model: "claude-opus-5".into(),
        },
    ]);
    assert_eq!(
        accepted.validate(OWNER),
        Ok(()),
        "a roster of distinct agents with one self and one advisor is the shape \
         CMA's own examples use"
    );
    assert_eq!(
        roster((0..20).map(|i| agent(&format!("worker{i}"))).collect()).validate(OWNER),
        Ok(()),
        "twenty entries is the documented maximum, not one past it"
    );
}
