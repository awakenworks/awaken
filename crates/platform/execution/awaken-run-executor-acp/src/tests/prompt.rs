//! Frozen ACP prompt ordering regressions, using the shared executor fixtures.
use super::*;

#[test]
fn acp_prompt_orders_frozen_policy_context_and_authoritative_input() {
    // Cause/effect graph: frozen instructions (C1), transient backend context
    // (C2), and durable current input (C3) produce ordered prompt sections (E1);
    // C2 does not mutate durable input (E2); without C1/C2, C3 passes through
    // byte-identically (E3). FMECA FM1: labelling C3 as untrusted made real ACP
    // agents refuse the current task in favor of quoted Plan context; explicitly
    // marking C3 authoritative under C1 mitigates that precedence inversion.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | PR1  | T  | T  | T  | E1+E2 |
    // | PR2  | F  | F  | T  | E3    |
    let mut activation = activation();
    activation.snapshot.resolved_spec.instructions = "follow policy".into();
    let original = activation.input.clone();
    let context = vec![
        Message::text(
            MessageId("session-baseline:historical".into()),
            Role::System,
            "obsolete resource revision",
        ),
        Message::text(
            MessageId("request-memory".into()),
            Role::System,
            "remember the user's preference",
        ),
    ];

    let prompt = super::initial_prompt(&activation, &context, &crate::AcpInstructions::default());

    let instructions = prompt.find("follow policy").expect("C1");
    let recalled = prompt.find("remember the user's preference").expect("C2");
    let input = prompt.find("do it").expect("C3");
    assert!(instructions < recalled && recalled < input, "R1: {prompt}");
    assert!(prompt.contains("authoritative task"), "R1: {prompt}");
    assert!(
        !prompt.contains("obsolete resource revision"),
        "R1: legacy derived context must not reach ACP"
    );
    assert_eq!(activation.input, original, "E2");

    activation.snapshot.resolved_spec.instructions.clear();
    assert_eq!(
        super::initial_prompt(&activation, &[], &crate::AcpInstructions::default()),
        "do it",
        "E3"
    );
}
