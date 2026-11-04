use super::*;
use axum::response::IntoResponse;
use axum::routing::get;

#[path = "zhike_customers.rs"]
mod customers;

struct Provider {
    wire: Mutex<ProviderWire>,
    state: Mutex<Conversation>,
    conversational: bool,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct ProviderWire {
    frames: Option<tokio::sync::mpsc::Sender<bytes::Bytes>>,
    history: Vec<String>,
    connections: usize,
}

#[derive(Default)]
struct Conversation {
    dialled: usize,
    ended: usize,
    voices: Vec<String>,
    assessment: Option<Value>,
    emitted: Vec<String>,
    current_turn: String,
    stale_replies: usize,
    task_results: usize,
    invited: bool,
    chat_messages: Vec<String>,
    customer_turns: Vec<String>,
    voice_sources: Vec<String>,
    customer_error: Option<String>,
    duplicate_voice_attempts: usize,
}

impl Provider {
    fn emit(&self, id: &str, kind: &str, text: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.emitted.push(id.into());
            if kind == "turn.completed" {
                state.current_turn = id.into();
            }
        }
        self.send(
            "notifications/phone/input",
            json!({"id":id,"kind":kind,"text":text}),
        );
    }

    fn send(&self, method: &str, params: Value) {
        let data = format!(
            "data: {}\n\n",
            json!({"jsonrpc":"2.0", "method":method,"params":params})
        );
        // Real MCP SSE input frames, deliberately split across UTF-8 codepoints.
        // This proves text-event transport, not streaming PCM or ASR accuracy.
        let mut wire = self.wire.lock().unwrap();
        wire.history.push(data.clone());
        let Some(frames) = &wire.frames else {
            return;
        };
        for chunk in data.as_bytes().chunks(7) {
            if frames
                .try_send(bytes::Bytes::copy_from_slice(chunk))
                .is_err()
            {
                break;
            }
        }
    }
}

async fn stream(State(provider): State<Arc<Provider>>) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel(1024);
    {
        let mut wire = provider.wire.lock().unwrap();
        wire.connections += 1;
        // Provider-side replay journal survives a connection loss. The MCP
        // notification protocol itself does not guarantee this replay service.
        for event in &wire.history {
            for chunk in event.as_bytes().chunks(7) {
                tx.try_send(bytes::Bytes::copy_from_slice(chunk)).unwrap();
            }
        }
        wire.frames = Some(tx);
    }
    (
        [("content-type", "text/event-stream")],
        Body::new(ProviderBody(rx)),
    )
}

struct ProviderBody(tokio::sync::mpsc::Receiver<bytes::Bytes>);
impl http_body::Body for ProviderBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|frame| frame.map(|bytes| Ok(http_body::Frame::data(bytes))))
    }
}

async fn rpc(
    State(provider): State<Arc<Provider>>,
    Json(request): Json<Value>,
) -> impl IntoResponse {
    let result = match request["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{},"tasks":{"requests":{"tools":{"call":{}}}}},"serverInfo":{"name":"zhike-push","version":"1"}})
        }
        "notifications/initialized" => return (StatusCode::ACCEPTED, Json(Value::Null)),
        "tools/list" => json!({"tools":[
            {"name":"StartCall","description":"Dial the fixture buyer once; wait for pushed call.connected before speaking.","execution":{"taskSupport":"required"},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
            {"name":"SendVoice","description":"Deliver your actual spoken reply. in_reply_to must be the latest source event_id; stale replies are rejected. Assistant text alone is not delivered.","inputSchema":{"type":"object","properties":{"in_reply_to":{"type":"string"},"text":{"type":"string"}},"required":["in_reply_to","text"],"additionalProperties":false}},
            {"name":"SubmitAssessment","description":"Record evidence-based qualification; unknown budget/score stay null. in_reply_to is the latest source event_id. Record human requests and do-not-contact explicitly.","inputSchema":{"type":"object","properties":{"in_reply_to":{"type":"string"},"budget":{"type":["number","null"]},"summary":{"type":"string"},"intent":{"type":"string","enum":["high","low","unknown"]},"score":{"type":["number","null"],"minimum":0,"maximum":1},"needs_human":{"type":"boolean"},"do_not_contact":{"type":"boolean"}},"required":["in_reply_to","budget","summary"],"additionalProperties":false}},
            {"name":"InviteContact","description":"Invite the buyer to official chat. An invitation is not proof of joining; await channel.joined after ending the phone call.","inputSchema":{"type":"object","properties":{"in_reply_to":{"type":"string"}},"required":["in_reply_to"],"additionalProperties":false}},
            {"name":"SendChatMessage","description":"Deliver to the verified conversation_id in channel.joined, continuing existing customer context.","inputSchema":{"type":"object","properties":{"in_reply_to":{"type":"string"},"conversation_id":{"type":"string"},"text":{"type":"string"}},"required":["in_reply_to","conversation_id","text"],"additionalProperties":false}},
            {"name":"EndCall","description":"End the phone call after assessment; in_reply_to is the latest source event_id.","execution":{"taskSupport":"required"},"inputSchema":{"type":"object","properties":{"in_reply_to":{"type":"string"}},"required":["in_reply_to"],"additionalProperties":false}}
        ]}),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap();
            let args = &request["params"]["arguments"];
            if name == "SendVoice" {
                let mut state = provider.state.lock().unwrap();
                if state
                    .voice_sources
                    .iter()
                    .any(|id| args["in_reply_to"] == *id)
                {
                    state.duplicate_voice_attempts += 1;
                    return (
                        StatusCode::OK,
                        Json(
                            json!({"jsonrpc":"2.0","id":request["id"],"result":{"isError":true,"content":[{"type":"text","text":"A complete response to this customer turn was already delivered. Do not repeat it; wait for a new customer turn."}]}}),
                        ),
                    );
                }
            }
            // Speech/qualification are tied to a turn. Hangup addresses the
            // already-owned call, not a text generation: a customer's final
            // goodbye must not invalidate an accepted request to end that call.
            if name != "StartCall" && name != "EndCall" {
                let mut state = provider.state.lock().unwrap();
                if args["in_reply_to"] != state.current_turn {
                    state.stale_replies += 1;
                    return (
                        StatusCode::OK,
                        Json(
                            json!({"jsonrpc":"2.0","id":request["id"],"result":{"isError":true,"content":[{"type":"text","text":"This response was superseded. Stop this turn; await the new customer input. No effect occurred."}]}}),
                        ),
                    );
                }
            }
            match name {
                "StartCall" => {
                    provider.state.lock().unwrap().dialled += 1;
                    provider.emit(
                        "connected",
                        "turn.completed",
                        "call.connected：电话已接通。",
                    );
                }
                "SendVoice" => {
                    let count = {
                        let mut state = provider.state.lock().unwrap();
                        assert_eq!(state.dialled, 1);
                        assert_eq!(state.ended, 0);
                        state.voices.push(args["text"].as_str().unwrap().into());
                        state
                            .voice_sources
                            .push(args["in_reply_to"].as_str().unwrap().into());
                        state.voices.len()
                    };
                    provider.changed.notify_one();
                    match count {
                        _ if provider.conversational => {},
                        1 => {
                            provider.emit("hypothesis", "partial", "我预算五百万元（未确认的识别假设）");
                            provider.emit("need", "turn.completed", "我们有十人客服团队，需要统一售前售后跟进。请简述产品再问预算。");
                            // EOF plus provider replay, while a new customer
                            // turn may already be in flight. No second input API.
                            provider.wire.lock().unwrap().frames.take();
                        }
                        2 => provider.emit("budget", "turn.completed", "预算五万元，本月采购，我负责采购。请通过 SendVoice 确认信息，然后邀请我添加官方聊天账号，结束电话后我们在那里继续。"),
                        _ => {},
                    }
                }
                "SubmitAssessment" => {
                    let mut state = provider.state.lock().unwrap();
                    assert!(
                        provider.conversational || state.voices.len() >= 3,
                        "qualification cannot substitute for the spoken confirmation"
                    );
                    state.assessment = Some(args.clone());
                }
                "InviteContact" => {
                    provider.state.lock().unwrap().invited = true;
                }
                "SendChatMessage" => {
                    let mut state = provider.state.lock().unwrap();
                    assert_eq!(args["conversation_id"], "chat-1");
                    assert!(state.assessment.is_some());
                    state
                        .chat_messages
                        .push(args["text"].as_str().unwrap().into());
                }
                "EndCall" => {
                    let mut state = provider.state.lock().unwrap();
                    assert!(provider.conversational || state.invited);
                    state.ended += 1;
                    drop(state);
                    provider.emit(
                        "ended",
                        "turn.completed",
                        "call.ended：通话已结束。不要再拨号或发送语音。",
                    );
                    // Provider-owned verification, never a customer assertion
                    // injected through the Session API. Real WeChat identity
                    // remains a separate provider qualification.
                    if !provider.conversational {
                        provider.emit("chat-joined", "turn.completed", "channel.joined：同一客户已通过邀请验证并加入官方聊天平台，conversation_id=chat-1。客户说：请沿用刚才的需求和最新预算，提交评估并在这里回复我，安排人工跟进。不要再拨电话。");
                    }
                    provider.changed.notify_one();
                }
                _ => panic!("unknown provider tool"),
            }
            if name == "StartCall" || name == "EndCall" {
                assert!(
                    request["params"]["task"].is_object(),
                    "phone lifecycle uses MCP Tasks"
                );
                let mut value = task("working");
                value["taskId"] = json!(name);
                json!({"task":value})
            } else {
                json!({"content":[{"type":"text","text":format!("{name} accepted")}]})
            }
        }
        "tasks/get" => {
            assert!(
                ["StartCall", "EndCall"].contains(&request["params"]["taskId"].as_str().unwrap())
            );
            let mut value = task("completed");
            value["taskId"] = request["params"]["taskId"].clone();
            value
        }
        "tasks/result" => {
            assert!(
                ["StartCall", "EndCall"].contains(&request["params"]["taskId"].as_str().unwrap())
            );
            provider.state.lock().unwrap().task_results += 1;
            json!({"content":[{"type":"text","text":"Phone operation completed."}]})
        }
        _ => panic!("unexpected MCP request"),
    };
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result})),
    )
}

#[derive(Default)]
struct InferenceGate {
    started: tokio::sync::Notify,
    cancelled: AtomicUsize,
    release: tokio::sync::Notify,
    returned: AtomicUsize,
    hard_cancel: bool,
}

struct CancellationWitness(Arc<InferenceGate>);
impl Drop for CancellationWitness {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}

struct ScriptedPush(Option<Arc<InferenceGate>>);
#[async_trait]
impl LlmExecutor for ScriptedPush {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last = request
            .messages
            .last()
            .unwrap()
            .content
            .iter()
            .filter_map(|c| {
                if let awaken_agent_contract::agent::content::ContentBlock::Text { text } = c {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<String>();
        let input_id = request
            .messages
            .iter()
            .rev()
            .filter(|m| m.role == awaken_agent_contract::agent::message::Role::User)
            .flat_map(|m| m.content.iter())
            .find_map(|c| {
                let awaken_agent_contract::agent::content::ContentBlock::Text { text } = c else {
                    return None;
                };
                let value: Value = serde_json::from_str(text).ok()?;
                value["event_id"].as_str().map(str::to_owned)
            })
            .unwrap_or_default();
        let call = |tool_id: &str, mut arguments: Value| {
            if tool_id != "mcp__phone__StartCall" {
                arguments["in_reply_to"] = json!(input_id);
            }
            let (tool_id, arguments) =
                if ["mcp__phone__StartCall", "mcp__phone__EndCall"].contains(&tool_id) {
                    (
                        "run_in_background",
                        json!({"tool":tool_id,"arguments":arguments}),
                    )
                } else {
                    (tool_id, arguments)
                };
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: format!("push-call-{}", request.messages.len()),
                tool_id: tool_id.into(),
                arguments,
            }])
        };
        let output = if last.contains("开始联系") {
            call("mcp__phone__StartCall", json!({}))
        } else if last.contains("call.connected") {
            call(
                "mcp__phone__SendVoice",
                json!({"text":"您好，我是知客，请问您需要解决什么问题？"}),
            )
        } else if last.contains("channel.joined") {
            let corrected = request.messages.iter().any(|m| {
                m.role == awaken_agent_contract::agent::message::Role::User
                    && serde_json::to_string(&m.content)
                        .unwrap()
                        .contains("三万元")
            });
            call(
                "mcp__phone__SubmitAssessment",
                json!({"budget":if corrected {30000} else {50000},"summary":"十人客服，本月采购，请人工跟进。"}),
            )
        } else if last.contains("十人客服") {
            call(
                "mcp__phone__SendVoice",
                json!({"text":"我们支持多渠道接待和人工跟进。您预算是多少？"}),
            )
        } else if last.contains("三万元") || last.contains("五万元") {
            let budget = if last.contains("三万元") {
                "三万元"
            } else {
                "五万元"
            };
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "voice-final".into(),
                tool_id: "mcp__phone__SendVoice".into(),
                arguments: json!({"in_reply_to":input_id,"text":format!("已记录您十人客服团队、{budget}预算和本月采购需求。")}),
            }])
        } else {
            // Tool results are nested blocks. Select the next exact action
            // from this request's latest tool result, not a provider backdoor.
            let raw = serde_json::to_string(&request.messages.last().unwrap().content).unwrap();
            if raw.contains("voice-final") && raw.contains("SendVoice accepted") {
                call("mcp__phone__InviteContact", json!({}))
            } else if raw.contains("InviteContact accepted") {
                call("mcp__phone__EndCall", json!({}))
            } else if raw.contains("SubmitAssessment accepted") {
                call(
                    "mcp__phone__SendChatMessage",
                    json!({"conversation_id":"chat-1","text":"已保留电话中的需求与最新预算，交给人工销售跟进；尚未成交。"}),
                )
            } else {
                AssistantOutput::text("等待后续事件。")
            }
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if let Some(gate) = &self.0 {
            let last = serde_json::to_string(&request.messages.last().unwrap().content).unwrap();
            if last.contains("五万元") && last.contains("event_id") {
                let _witness = CancellationWitness(gate.clone());
                // Injection point: tool arguments have started streaming, but
                // the incomplete JSON must never execute as a speech command.
                let response = self.infer(request).await?;
                let call = &response.output.tool_calls()[0];
                let arguments = call.arguments.to_string();
                sink.on_tool_call_delta(&call.call_id, &call.tool_id, &arguments[..10])
                    .await;
                gate.started.notify_one();
                gate.release.notified().await;
                sink.on_tool_call_delta(&call.call_id, &call.tool_id, &arguments[10..])
                    .await;
                gate.returned.fetch_add(1, Ordering::SeqCst);
                return Ok(response);
            }
        }
        let response = self.infer(request).await?;
        for call in response.output.tool_calls() {
            for c in call.arguments.to_string().chars() {
                sink.on_tool_call_delta(&call.call_id, &call.tool_id, &c.to_string())
                    .await;
            }
        }
        Ok(response)
    }
}

async fn scenario(
    executor: Arc<dyn LlmExecutor>,
    model_name: &str,
    interrupt: Option<Arc<InferenceGate>>,
    customer: Option<customers::CustomerCase>,
) {
    let provider = Arc::new(Provider {
        wire: Mutex::new(ProviderWire::default()),
        state: Mutex::new(Conversation::default()),
        conversational: customer.is_some(),
        changed: tokio::sync::Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server_app = Router::new()
        .route("/", get(stream).post(rpc))
        .with_state(provider.clone());
    let server = tokio::spawn(async move { axum::serve(listener, server_app).await.unwrap() });
    let customer_driver = customer
        .map(|case| customers::spawn(provider.clone(), executor.clone(), model_name.into(), case));
    let interrupt_driver = interrupt.clone().map(|gate| {
        let provider = provider.clone();
        tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(60), gate.started.notified())
                .await
                .expect("old generation entered");
            // Service-side turn invalidation happens immediately. Unlike a
            // hard model cancellation, it need not claim remote termination.
            provider.state.lock().unwrap().current_turn = "interrupted".into();
            if gate.hard_cancel {
                provider.send(
                    "notifications/phone/interrupt",
                    json!({"id":"interrupt-1","input_event_id":"budget"}),
                );
            }
            provider.emit(
                "budget-correction",
                "turn.completed",
                "刚才说错了，预算改为三万元，本月采购。请确认更正，邀请我添加官方聊天账号，结束电话后在那里继续。",
            );
            if gate.hard_cancel {
                provider.send(
                    "notifications/phone/interrupt",
                    json!({"id":"interrupt-1","input_event_id":"budget"}),
                );
            } else {
                gate.release.notify_one();
            }
        })
    });
    let (app, lifecycle) = awaken_cli::build_all_in_one_service_with_host_customizer(
        executor,
        awaken_runtime_contract::resolved::ModelBinding::parse("default", model_name, "genai")
            .unwrap(),
        awaken_deployment_config::DeploymentConfig {
            sandbox_tier: awaken_deployment_config::SandboxTier::Local,
            ..awaken_deployment_config::DeploymentConfig::ephemeral()
        },
        None,
        |host| host,
        false,
    )
    .await;
    let config = json!({"id":"zhike-push","name":"知客","system":include_str!("zhike-sales-system.txt"),
    "max_steps":8,"model":{"mode":"pinned","provider_identity_ref":"default","model_ref":model_name,"backend_ref":"genai"},
    "mcp_servers":[{"name":"phone","type":"url","url":url}],
    "tools":[{"type":"mcp_toolset","mcp_server_name":"phone","default_config":{"enabled":true,"permission_policy":{"type":"always_allow"}}}],
        "plugins":["mcp:phone","background_task","state_machine"],
        "plugin_config":{
            "background_task":{"tools":["mcp__phone__StartCall","mcp__phone__EndCall"],"notifications":{"default":{"enabled":false}}},
            "state_machine":{"machines":[{"name":"one-call-command","scope":"thread","key":"{tool}","initial":"ready","terminal":["submitted"],"transitions":[{"on":"run_in_background","from":"ready","to":"submitted","when":"success","on_violation":{"action":"deny","reason":"This call command was already submitted; wait for its provider event."}}]}]},
            "mcp:phone":{"notifications":{
        "notifications/phone/input":{"event_id":"/id","delivery":{"kind":"message","text":"/text"},"when":{"/kind":"turn.completed"}},
        "notifications/phone/interrupt":{"event_id":"/id","delivery":{"kind":"cancel_response","input_method":"notifications/phone/input","input_event_id":"/input_event_id"}}
    }}}});
    assert_eq!(
        api(&app, "PUT", "/v1/config/agents/zhike-push", config)
            .await
            .0,
        StatusCode::OK
    );
    let (status, publication) = api(
        &app,
        "POST",
        "/v1/config/agents/zhike-push/publish",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{publication}");
    let (status, session) = api(
        &app,
        "POST",
        "/v1/sessions",
        json!({"agent":"zhike-push","environment_id":"env_local"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["id"].as_str().unwrap();
    // Sole test-authored input is task commissioning, NOT a customer utterance.
    // All subsequent customer content must originate in the provider SSE stream.
    let (status, receipt) = api(&app,"POST",&format!("/v1/sessions/{session_id}/events"),json!({"events":[{"type":"user.message","content":[{"type":"text","text":"开始联系客户。"}]}]})).await;
    assert!(status.is_success(), "{receipt}");
    let read = app.clone();
    let path = format!("/v1/sessions/{session_id}/events?limit=500");
    let hard_cancel = interrupt.as_ref().is_some_and(|g| g.hard_cancel);
    awaken_protocol_managed_testkit::wait_for_session_events(
        DurableFact::command_processed(&receipt),
        Duration::from_secs(90),
        "MCP provider dialogue reaches physical hangup",
        move || {
            let app = read.clone();
            let path = path.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let (s, v) = api(&app, "GET", &path, Value::Null).await;
                (s.as_u16(), v)
            }
        },
        |events| {
            if customer.is_some() {
                let state = provider.state.lock().unwrap();
                state.customer_error.is_some()
                    || (state.ended == 1 && state.assessment.is_some() && state.task_results >= 2)
            } else if hard_cancel {
                events.iter().any(|e| {
                    e["type"] == "session.error"
                        && e.to_string().contains("execution result is unknown")
                })
            } else {
                let state = provider.state.lock().unwrap();
                state.ended == 1
                    && state.chat_messages.len() == 1
                    && state.task_results >= 2
                    && provider.wire.lock().unwrap().connections >= 2
            }
        },
    )
    .await;
    // One guard per assertion region. The lock is an uncontended test fixture,
    // so re-taking it costs nothing and makes "no guard is held across an
    // await" a property of the shape rather than of remembering to drop.
    assert_eq!(provider.state.lock().unwrap().dialled, 1);
    if let Some(case) = customer {
        let (mut verdict, voices) = {
            let state = provider.state.lock().unwrap();
            (customers::evaluate(case, &state), state.voices.clone())
        };
        if let Some(driver) = customer_driver {
            driver.await.unwrap();
        }
        match customers::check_grounding(&app, model_name, &voices).await {
            Ok(grade) if grade.decision == awaken_ext_goal::outcome::GradeDecision::Satisfied => {}
            Ok(grade) => verdict.push(format!("product_grounding: {}", grade.explanation)),
            Err(error) => verdict.push(format!("grounding_evaluation_unavailable: {error}")),
        }
        eprintln!("ZHike overall {:?}: {}", case, json!({"failures":verdict}));
        lifecycle.shutdown(Duration::from_secs(10)).await.unwrap();
        server.abort();
        assert!(verdict.is_empty(), "{case:?}: {}", verdict.join("; "));
        return;
    }
    assert!(
        provider.wire.lock().unwrap().connections >= 2,
        "real SSE reconnect observed"
    );
    if hard_cancel {
        // A local cancelled Future is not a remote terminal receipt. This is
        // a fail-closed safety case, NOT successful conversational continuation.
        {
            let state = provider.state.lock().unwrap();
            assert_eq!(state.voices.len(), 2);
            assert_eq!(state.ended, 0);
            assert!(state.assessment.is_none());
        }
        assert_eq!(
            interrupt.as_ref().unwrap().returned.load(Ordering::SeqCst),
            0
        );
        if let Some(driver) = interrupt_driver {
            driver.await.unwrap();
        }
        // I4 has no physical terminal receipt. The original drain cannot claim
        // quiescence; it must report exactly that registered task at the same
        // deadline, while the committed Unknown and physical facts stay intact.
        let shutdown = lifecycle
            .shutdown(Duration::from_secs(10))
            .await
            .expect_err("I4 unknown physical execution cannot become clean drain");
        assert_eq!(shutdown.timed_out, ["coordinator-runtime-background-drain"]);
        let (status, history) = api(
            &app,
            "GET",
            &format!("/v1/sessions/{session_id}/events?limit=500"),
            Value::Null,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "I4 original committed history remains readable"
        );
        assert!(
            history["data"].as_array().unwrap().iter().any(|event| {
                event["type"] == "session.error"
                    && event.to_string().contains("execution result is unknown")
            }),
            "I4 shutdown does not synthesize terminal success"
        );
        {
            let state = provider.state.lock().unwrap();
            assert_eq!(state.voices.len(), 2, "I4 no replacement speech");
            assert_eq!(state.ended, 0, "I4 no invented physical hangup");
            assert!(state.assessment.is_none(), "I4 no post-shutdown assessment");
        }
        server.abort();
        return;
    }
    {
        let state = provider.state.lock().unwrap();
        assert_eq!(state.voices.len(), 3);
        assert_eq!(state.chat_messages.len(), 1);
        assert_eq!(
            state.assessment.as_ref().unwrap()["budget"],
            if interrupt.is_some() { 30000 } else { 50000 }
        );
        assert!(!state.voices.iter().any(|s| s.contains("五百万元")));
        if let Some(gate) = interrupt {
            assert_eq!(
                gate.cancelled.load(Ordering::SeqCst),
                1,
                "old inference future relinquished"
            );
            assert_eq!(
                gate.returned.load(Ordering::SeqCst),
                1,
                "old inference reached a real return"
            );
            assert_eq!(
                state.stale_replies, 1,
                "provider rejected the obsolete completed tool call"
            );
            assert!(
                !state.voices.iter().any(|s| s.contains("五万元")),
                "old partial tool call never played"
            );
        }
    }
    if let Some(driver) = interrupt_driver {
        driver.await.unwrap();
    }
    lifecycle.shutdown(Duration::from_secs(10)).await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_pushed_customer_inputs_produce_delivered_voice_replies() {
    // P1 initial commission -> physical dial -> MCP connected event -> spoken
    // opening -> two MCP final utterances -> three voice receipts -> assessment
    // -> hangup. P2 partial hypothesis must never replace confirmed budget.
    // No test-side customer Session POST, no direct handler invocation and no
    // observer-only model text counts as speech delivery. This is still not
    // Flow Issue acceptance, PCM/TTS playback, or real carrier qualification.
    scenario(
        Arc::new(ScriptedPush(None)),
        "zhike-push-fixture",
        None,
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit DeepSeek credential and network"]
async fn live_deepseek_mcp_pushed_customer_inputs() {
    let (executor, model) = deepseek_executor();
    scenario(executor, &model, None, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_turn_supersession_during_tool_argument_stream_rejects_old_playback() {
    // I1 partial old tool JSON + provider turn change -> old inference drains
    // to a terminal response but old voice has no effect. I2 corrected final ->
    // corrected reply and assessment. No local drop is called remote shutdown.
    // Only the model timing is deterministic; ingress, Run control and MCP
    // effects use production paths, with no customer Session POST from tests.
    let gate = Arc::new(InferenceGate::default());
    scenario(
        Arc::new(ScriptedPush(Some(gate.clone()))),
        "zhike-push-fixture",
        Some(gate),
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_hard_cancel_without_provider_terminal_receipt_remains_indeterminate() {
    // I4 explicit exact cancellation during inference with no terminal receipt
    // -> unknown execution and no automatic redial/speech/assessment. This is
    // deliberately separate from the successful turn-supersession acceptance.
    // With explicit lifecycle teardown, that same unresolved physical work must
    // report only its named drain timeout, never become a clean terminal result;
    // the original history and provider facts are checked again after teardown.
    let gate = Arc::new(InferenceGate {
        hard_cancel: true,
        ..Default::default()
    });
    scenario(
        Arc::new(ScriptedPush(Some(gate.clone()))),
        "zhike-push-fixture",
        Some(gate),
        None,
    )
    .await;
}
