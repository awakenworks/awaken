use awaken_server::protocols::a2a::http::{AgentCapabilities, AgentCard};

#[test]
fn well_known_card_shape() {
    let card = AgentCard {
        id: "agent".into(),
        name: "agent".into(),
        description: Some("d".into()),
        url: "/v1/a2a/agents/agent/message:send".into(),
        version: "0.1.0".into(),
        capabilities: Some(AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        }),
        skills: vec![],
    };
    let v = serde_json::to_value(card).unwrap();
    assert_eq!(v["name"], "agent");
    assert_eq!(v["capabilities"]["streaming"], true);
}
