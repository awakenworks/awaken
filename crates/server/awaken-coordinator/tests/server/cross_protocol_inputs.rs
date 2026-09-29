//! Cross-interface input history on the same Session and production Run application.

use super::{
    Body, BodyExt, Request, ServiceExt, StatusCode, build_custom_router, create_session, json_call,
};

#[tokio::test]
async fn ai_sdk_input_and_client_answer_remain_one_managed_history_each() {
    // Graph: C1 a Managed-created Session; C2 AI SDK submits a new customer
    // message; C3 the real kernel awaits a declared client tool; C4 AI SDK
    // supplies its answer; C5 the same Managed history is read again.
    // R1 C1+C2 -> one byte-faithful user.message before the model response.
    // R2 C3+C4 -> one user.custom_tool_result naming the same public call,
    // not an agent.tool_result, and the model receives the answer.
    // R3 C5 -> identical event IDs/content, no extra input or execution.
    // Constraints: this is deterministic protocol/commit coverage, not real
    // model quality. The existing server helpers and custom-tool fixture own
    // setup. No parallel input ledger or application-specific event API.
    let app = build_custom_router();
    let session = create_session(&app).await;
    let customer = "服务费金额本次没提供，收款时间还没定。";
    let mut payloads = vec![serde_json::json!({
        "messages": [{"id":"customer-input", "role":"user",
            "parts":[{"type":"text", "text":customer}]}]
    })];
    let mut first = None;
    let mut public_call = String::new();
    for turn in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/ai-sdk/threads/{session}/runs"))
                    .header("content-type", "application/json")
                    .body(Body::from(payloads[turn].to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            response.into_body().collect(),
        )
        .await
        .expect("bounded SDK turn")
        .unwrap()
        .to_bytes();
        let wire = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!wire.contains("\"type\":\"error\""), "{wire}");
        if turn == 0 {
            let managed = json_call(
                &app,
                "GET",
                &format!("/v1/sessions/{session}/events"),
                serde_json::Value::Null,
            )
            .await;
            public_call = managed["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["type"] == "agent.custom_tool_use")
                .expect("real kernel reaches client tool")["id"]
                .as_str()
                .unwrap()
                .to_owned();
            first = Some(managed);
            let history = json_call(
                &app,
                "GET",
                &format!("/v1/ai-sdk/threads/{session}/messages"),
                serde_json::Value::Null,
            )
            .await;
            let mut messages = history["items"].as_array().unwrap().clone();
            let part = messages
                .iter_mut()
                .flat_map(|message| message["parts"].as_array_mut().unwrap())
                .find(|part| part["toolName"] == "submit_answer")
                .expect("SDK client tool");
            part["state"] = "output-available".into();
            part["output"] = "42".into();
            payloads.push(serde_json::json!({"messages":messages}));
        }
    }
    let first = first.unwrap();
    let users: Vec<_> = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "user.message")
        .collect();
    assert_eq!(users.len(), 1, "R1 user input must not disappear: {first}");
    assert_eq!(
        users[0]["content"],
        serde_json::json!([{"type":"text", "text":customer}]),
        "R1 exact certainty"
    );
    let final_history = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        serde_json::Value::Null,
    )
    .await;
    let events = final_history["data"].as_array().unwrap();
    let answers: Vec<_> = events
        .iter()
        .filter(|event| {
            event["type"] == "user.custom_tool_result" && event["custom_tool_use_id"] == public_call
        })
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "R2 one public client answer: {final_history}"
    );
    assert_eq!(
        answers[0]["content"],
        serde_json::json!([{"type":"text", "text":"42"}])
    );
    assert!(
        !events.iter().any(
            |event| event["type"] == "agent.tool_result" && event["tool_use_id"] == public_call
        ),
        "R2 correct result family"
    );
    assert!(
        events.iter().any(|event| event["type"] == "agent.message"
            && event["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("42"))),
        "R2 model consumes answer"
    );
    assert_eq!(
        final_history,
        json_call(
            &app,
            "GET",
            &format!("/v1/sessions/{session}/events"),
            serde_json::Value::Null
        )
        .await,
        "R3 stable warm read"
    );
}
