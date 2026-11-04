//! Cross-interface input history on the same Session and production Run application.

use super::{
    Body, BodyExt, DurableFact, Request, ServiceExt, StatusCode, build_custom_router,
    create_session, json_call, wait_for_session_events,
};

#[tokio::test]
async fn ai_sdk_input_and_client_answer_remain_one_managed_history_each() {
    // O1/O7: keep each selected fixture owner through the original assertions;
    // protocol clones share it, while independent constructors retain separate owners.
    let fixture_lifecycle_1 = awaken_service_lifecycle::ServiceLifecycle::new();

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
    // C6 a racing Managed refresh may legally serve its prior complete prefix.
    // R4 C6 -> observe with the existing read-only fact waiter until the fresh
    // Session's exact input and client-tool fact are visible. R5 the answer's
    // processed_at plus the model's 42 response -> the original stable-history
    // assertions. The SDK stream itself must already expose submit_answer.
    // No Managed command receipt exists for these SDK inputs; this fresh
    // Session has only this command, and the refinements name its exact content.
    let app = build_custom_router(false, fixture_lifecycle_1.clone()).await;
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
            assert!(wire.contains("\"type\":\"tool-input-available\""), "{wire}");
            assert!(wire.contains("\"toolName\":\"submit_answer\""), "{wire}");
            let managed = wait_for_session_events(
                &app,
                &session,
                DurableFact::committed_event("agent.custom_tool_use"),
                "the SDK command's exact input and client tool",
                |events| {
                    events.iter().any(|event| {
                        event["type"] == "user.message"
                            && event["content"]
                                == serde_json::json!([{"type":"text", "text":customer}])
                    })
                },
            )
            .await;
            public_call = managed["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["type"] == "agent.custom_tool_use")
                .unwrap_or_else(|| {
                    panic!("real kernel reaches client tool; sdk={wire}; managed={managed}")
                })["id"]
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
    let final_history = wait_for_session_events(
        &app,
        &session,
        DurableFact::committed_event("user.custom_tool_result"),
        "the exact client answer to be processed and consumed by the model",
        |events| {
            events.iter().any(|event| {
                event["type"] == "user.custom_tool_result"
                    && event["custom_tool_use_id"] == public_call
                    && event["processed_at"].is_string()
            }) && events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("42"))
            })
        },
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

    fixture_lifecycle_1
        .shutdown(std::time::Duration::from_secs(10))
        .await
        .expect("O1 selected fixture joins its registered work");
}
