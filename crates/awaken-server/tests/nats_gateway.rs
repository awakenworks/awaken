#![cfg(feature = "nats")]

use awaken_server::transport::nats::{NatsRunMessage, NatsRunRequest, convert_nats_messages};

#[test]
fn nats_request_deserialize_and_convert() {
    let req: NatsRunRequest = serde_json::from_value(serde_json::json!({
        "thread_id": "t1",
        "agent_id": "a1",
        "messages": [{"role":"user","content":"hi"}]
    }))
    .unwrap();
    let msgs = convert_nats_messages(req.messages);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "hi");
}

#[test]
fn nats_unknown_role_filtered() {
    let msgs = convert_nats_messages(vec![NatsRunMessage {
        role: "x".into(),
        content: "c".into(),
    }]);
    assert!(msgs.is_empty());
}
