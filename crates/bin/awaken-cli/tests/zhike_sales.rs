//! Zhike sales acceptance through the real Control/Session/Runtime composition.
//! Only the telephone/chat provider is simulated. The ignored variant uses a
//! real explicitly materialized DeepSeek executor with the identical scenario.

use async_trait::async_trait;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, DeltaSink, LlmExecutor, ToolCall,
};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    routing::post,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tower::ServiceExt;

struct FragmentedInput {
    parts: std::collections::VecDeque<bytes::Bytes>,
    observed: Arc<AtomicUsize>,
}

impl http_body::Body for FragmentedInput {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::task::Poll::Ready(self.parts.pop_front().map(|part| {
            self.observed.fetch_add(1, Ordering::SeqCst);
            Ok(http_body::Frame::data(part))
        }))
    }
}

#[derive(Default)]
struct Phone {
    starts: usize,
    ends: usize,
    polls: usize,
    result_reads: usize,
    events: Vec<String>,
    invited: bool,
    assessment: Option<Value>,
}

fn task(status: &str) -> Value {
    json!({"taskId":"phone-connect-task","status":status,
        "createdAt":"2026-09-13T00:00:00Z","lastUpdatedAt":"2026-09-13T00:00:01Z",
        "ttl":60000,"pollInterval":25})
}

async fn phone_rpc(
    State(state): State<Arc<Mutex<Phone>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let mut phone = state.lock().unwrap();
    let result = match request["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tasks":{"requests":{"tools":{"call":{}}},"cancel":{}}},"serverInfo":{"name":"phone","version":"1"}})
        }
        "tools/list" => json!({"tools":[
            {"name":"StartCall","description":"Dial the fixture buyer; background result confirms connection. Never call twice.","execution":{"taskSupport":"required"},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
            {"name":"EndCall","description":"End the current mock call through a background task; completion confirms call.ended.","execution":{"taskSupport":"required"},"inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
            {"name":"InviteContact","description":"Invite this buyer to the official second chat channel. This does not prove they joined.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
            {"name":"SubmitAssessment","description":"Submit evidence-based sales qualification after inviting the buyer. Unknown budgets remain null. Score is intent, not calibrated purchase probability.","inputSchema":{"type":"object","properties":{"intent":{"type":"string","enum":["high","low","unknown"]},"score":{"type":"number","minimum":0,"maximum":1},"budget":{"type":["number","null"]},"summary":{"type":"string"}},"required":["intent","score","budget","summary"],"additionalProperties":false}}
        ]}),
        "tools/call" => match request["params"]["name"].as_str().unwrap_or("") {
            "StartCall" => {
                phone.starts += 1;
                json!({"task":task("working")})
            }
            "EndCall" => {
                assert_eq!(phone.starts, 1, "hangup has an existing call");
                phone.ends += 1;
                let mut ending = task("working");
                ending["taskId"] = json!("phone-end-task");
                json!({"task":ending})
            }
            "InviteContact" => {
                phone.invited = true;
                json!({"content":[{"type":"text","text":"Invitation sent to the mock official chat; joining is not yet confirmed."}]})
            }
            "SubmitAssessment" => {
                assert!(
                    phone.invited,
                    "state-machine must block qualification before invitation"
                );
                phone.assessment = Some(request["params"]["arguments"].clone());
                json!({"content":[{"type":"text","text":"Qualification recorded. Human sales follow-up is requested, not completed."}]})
            }
            _ => panic!("unknown fixture tool"),
        },
        "tasks/get" => {
            assert!(
                ["phone-connect-task", "phone-end-task"]
                    .contains(&request["params"]["taskId"].as_str().unwrap())
            );
            phone.polls += 1;
            let mut observed = task("completed");
            observed["taskId"] = request["params"]["taskId"].clone();
            observed
        }
        "tasks/result" => {
            assert!(
                ["phone-connect-task", "phone-end-task"]
                    .contains(&request["params"]["taskId"].as_str().unwrap())
            );
            phone.result_reads += 1;
            let event = if request["params"]["taskId"] == "phone-end-task" {
                "call.ended"
            } else {
                "call.connected"
            };
            phone.events.push(event.into());
            json!({"content":[{"type":"text","text":json!({"event":event,"call_id":"mock-call-1","buyer":"林先生"}).to_string()}]})
        }
        "tasks/cancel" => task("cancelled"),
        "notifications/initialized" => json!({}),
        _ => json!({}),
    };
    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
}

async fn api(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    // Input-stream decision rule: Event JSON arrives in multiple transport
    // frames (including UTF-8 splits); Axum must assemble it before admitting
    // the customer turn. This does not claim partial-ASR or audio support.
    let fragments = Arc::new(AtomicUsize::new(0));
    let fragmented = method == "POST" && path.ends_with("/events");
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("anthropic-beta", "managed-agents-2026-04-01");
    if !body.is_null() {
        request = request.header("content-type", "application/json");
    }
    let request = request
        .body(if body.is_null() {
            Body::empty()
        } else if fragmented {
            Body::new(FragmentedInput {
                parts: body
                    .to_string()
                    .as_bytes()
                    .chunks(17)
                    .map(bytes::Bytes::copy_from_slice)
                    .collect(),
                observed: fragments.clone(),
            })
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    if fragmented {
        assert!(
            fragments.load(Ordering::SeqCst) > 1,
            "customer input crossed multiple body frames"
        );
    }
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}

struct ObservedModel {
    inner: Arc<dyn LlmExecutor>,
    text_chunks: AtomicUsize,
    tool_chunks: AtomicUsize,
    result_lookups: AtomicUsize,
    connected_inputs: AtomicUsize,
}

struct ObserveDeltas<'a> {
    model: &'a ObservedModel,
    next: &'a dyn DeltaSink,
}

#[async_trait]
impl DeltaSink for ObserveDeltas<'_> {
    async fn on_text(&self, text: &str) {
        self.model.text_chunks.fetch_add(1, Ordering::SeqCst);
        self.next.on_text(text).await;
    }
    async fn on_reasoning(&self, text: &str) {
        self.next.on_reasoning(text).await;
    }
    async fn on_tool_call_delta(&self, call: &str, tool: &str, delta: &str) {
        self.model.tool_chunks.fetch_add(1, Ordering::SeqCst);
        self.next.on_tool_call_delta(call, tool, delta).await;
    }
}

#[async_trait]
impl LlmExecutor for ObservedModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.inner.infer(request).await
    }
    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if request.messages.iter().any(|m| m.content.iter().any(|b| matches!(b, awaken_runtime_contract::ContentBlock::Text { text } if text.contains("call.connected") && text.contains("background_task")))) {
            self.connected_inputs.fetch_add(1, Ordering::SeqCst);
        }
        let response = self
            .inner
            .infer_streaming(
                request,
                &ObserveDeltas {
                    model: self,
                    next: sink,
                },
            )
            .await?;
        self.result_lookups.fetch_add(
            response
                .output
                .tool_calls()
                .iter()
                .filter(|t| t.tool_id == "get_background_task")
                .count(),
            Ordering::SeqCst,
        );
        Ok(response)
    }
}

struct ScriptedSales;
#[async_trait]
impl LlmExecutor for ScriptedSales {
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
            .filter_map(|b| match b {
                awaken_runtime_contract::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let tool = |id: &str, arguments| {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: format!("tool-{}", request.messages.len()),
                tool_id: id.into(),
                arguments,
            }])
        };
        let output = if last.contains("开始联系") {
            tool(
                "run_in_background",
                json!({"tool":"mcp__phone__StartCall","arguments":{}}),
            )
        } else if last.contains("提交评估") {
            tool(
                "mcp__phone__SubmitAssessment",
                json!({"intent":"high","score":0.9,"budget":50000,"summary":"买家确认客服需求、五万元预算和本月采购；请求人工演示。"}),
            )
        } else if last.contains("请结束电话") {
            tool(
                "run_in_background",
                json!({"tool":"mcp__phone__EndCall","arguments":{}}),
            )
        } else if last.contains("请邀请") {
            tool("mcp__phone__InviteContact", json!({}))
        } else if last.contains("call.connected") {
            AssistantOutput::text("您好林先生，我是知客。请问您想改善客服接待还是售后跟进？")
        } else if last.contains("十人") {
            AssistantOutput::text(
                "了解，您的十人客服团队可以先评估多渠道接待与任务跟进。您计划什么时候采购？",
            )
        } else {
            AssistantOutput::text(
                "已记录。我会依据您提供的需求继续跟进，尚未确认的事项保持待确认。",
            )
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
        let response = self.infer(request).await?;
        for call in response.output.tool_calls() {
            for part in call
                .arguments
                .to_string()
                .chars()
                .collect::<Vec<_>>()
                .chunks(8)
            {
                sink.on_tool_call_delta(
                    &call.call_id,
                    &call.tool_id,
                    &part.iter().collect::<String>(),
                )
                .await;
            }
        }
        for ch in response.output.text_content().chars() {
            sink.on_text(&ch.to_string()).await;
        }
        Ok(response)
    }
}

async fn send_and_wait(app: &Router, session: &str, text: &str) -> Vec<Value> {
    let (status, receipt) = api(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        json!({"events":[{"type":"user.message","content":[{"type":"text","text":text}]}]}),
    )
    .await;
    assert!(status.is_success(), "message admission: {status} {receipt}");
    let app = app.clone();
    let path = format!("/v1/sessions/{session}/events?limit=500");
    let list = awaken_protocol_managed::test_support::wait_for_session_events(
        Some(&receipt),
        Duration::from_secs(120),
        "Zhike turn completes",
        move || {
            let app = app.clone();
            let path = path.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let (s, v) = api(&app, "GET", &path, Value::Null).await;
                (s.as_u16(), v)
            }
        },
        |events| events.iter().any(|e| e["type"] == "session.status_idle"),
    )
    .await;
    list["data"].as_array().unwrap().clone()
}

async fn sales_scenario(inner: Arc<dyn LlmExecutor>, model_name: &str, verify_guard: bool) {
    let phone = Arc::new(Mutex::new(Phone::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let phone_app = Router::new()
        .route("/", post(phone_rpc))
        .with_state(phone.clone());
    let server = tokio::spawn(async move { axum::serve(listener, phone_app).await.unwrap() });
    let model = Arc::new(ObservedModel {
        inner,
        text_chunks: AtomicUsize::new(0),
        tool_chunks: AtomicUsize::new(0),
        result_lookups: AtomicUsize::new(0),
        connected_inputs: AtomicUsize::new(0),
    });
    // This scenario uses only network MCP tools. Select the documented local
    // fixture substrate explicitly; it does not exercise namespace Hand tools.
    let app = awaken_cli::build_all_in_one_router_with_host_customizer(
        model.clone(),
        awaken_runtime_contract::resolved::ModelBinding::parse("default", model_name, "genai")
            .expect("complete fixture model binding"),
        awaken_session_executor::DeploymentConfig {
            sandbox_tier: awaken_session_executor::SandboxTier::Local,
            ..awaken_session_executor::DeploymentConfig::ephemeral()
        },
        None,
        |host| host,
    )
    .await;
    let config = json!({"id":"zhike","name":"知客","system":"你是知客，模拟产品的销售顾问。产品资料：支持多渠道接待、任务跟进和人工交接；未提供价格和效果保证。保持简短自然，每次只问一个关键问题。开始联系时只通过 run_in_background 调用 mcp__phone__StartCall 一次，然后结束本轮等待通知。后台结果会直接注入，不调用 get_background_task。收到 call.connected 才开场。保留客户给出的团队规模、需求、预算和采购时间。只有客户要求时调用 InviteContact；邀请已发送不等于已经添加。需要提交评估时调用 SubmitAssessment，预算未披露用 null，评分只是意向不是成交概率。切换到第二平台后沿用本任务上下文，不重复询问已确认信息。",
    "max_steps":8,"model":{"mode":"pinned","provider_identity_ref":"default","model_ref":model_name,"backend_ref":"genai"},
    "mcp_servers":[{"name":"phone","type":"url","url":url}],
    "tools":[{"type":"mcp_toolset","mcp_server_name":"phone","default_config":{"enabled":true,"permission_policy":{"type":"always_allow"}}}],
    "tool_exposure":{"rules":[
        {"selector":{"kind":"exact","value":"get_background_task"},"exposure":"on_demand"},
        {"selector":{"kind":"exact","value":"list_background_tasks"},"exposure":"on_demand"}
    ]},
    "plugins":["background_task","state_machine"],"plugin_config":{
            "background_task":{"tools":["mcp__phone__StartCall","mcp__phone__EndCall"],"notifications":{"default":{"format":"tagged"}}},
        "state_machine":{"machines":[{"name":"contact-before-qualification","scope":"thread","key":"","initial":"discovery","terminal":["qualified"],"transitions":[
            {"on":"mcp__phone__InviteContact","from":["discovery","invited"],"to":"invited","when":"success"},
            {"on":"mcp__phone__SubmitAssessment","from":["invited","qualified"],"to":"qualified","when":"success","on_violation":{"action":"deny","reason":"Invite the buyer before final qualification."}}
        ]},{"name":"one-call-command","scope":"thread","key":"{tool}","initial":"ready","terminal":["submitted"],"transitions":[
            {"on":"run_in_background","from":"ready","to":"submitted","when":"success","on_violation":{"action":"deny","reason":"This call command was already submitted. Wait for its event; do not submit it again."}}
        ]}]}
    }});
    let (status, value) = api(&app, "PUT", "/v1/config/agents/zhike", config).await;
    assert_eq!(status, StatusCode::OK, "author: {value}");
    let (status, value) = api(&app, "POST", "/v1/config/agents/zhike/publish", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "publish: {value}");
    let (status, value) = api(
        &app,
        "POST",
        "/v1/sessions",
        json!({"agent":"zhike","environment_id":"env_local"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "session: {value}");
    let session = value["id"].as_str().unwrap();
    send_and_wait(&app, session, "开始联系买家林先生。").await;
    // Poll only public history; the real Host observer owns task launch and
    // Session attention. No test call to get_background_task or task driver.
    let app_read = app.clone();
    let path = format!("/v1/sessions/{session}/events?limit=500");
    awaken_protocol_managed::test_support::wait_for_session_events(
        None,
        Duration::from_secs(120),
        "connected notification produces greeting",
        move || {
            let app = app_read.clone();
            let path = path.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let (s, v) = api(&app, "GET", &path, Value::Null).await;
                (s.as_u16(), v)
            }
        },
        |events| {
            events
                .iter()
                .any(|e| e["type"] == "agent.message" && e.to_string().contains("知客"))
        },
    )
    .await;
    if verify_guard {
        let rejected = send_and_wait(&app, session, "请提前提交评估。").await;
        assert!(
            phone.lock().unwrap().assessment.is_none(),
            "state machine prevents physical early qualification"
        );
        assert!(
            rejected.iter().any(|event| event
                .to_string()
                .contains("Invite the buyer before final qualification")),
            "state-machine refusal reaches model history"
        );
    }
    send_and_wait(
        &app,
        session,
        "我们有十人客服团队，需要把售前和售后跟进统一起来。",
    )
    .await;
    send_and_wait(
        &app,
        session,
        "预算五万元，本月采购。我负责采购，请邀请我添加官方聊天账号。",
    )
    .await;
    assert!(
        phone.lock().unwrap().invited,
        "invitation emitted through real MCP"
    );
    send_and_wait(&app, session, "请结束电话，我们到官方聊天平台继续。").await;
    if verify_guard {
        let duplicate = send_and_wait(&app, session, "请结束电话。").await;
        assert!(
            duplicate.iter().any(|event| event
                .to_string()
                .contains("This call command was already submitted")),
            "repeated hangup is denied by the state-machine extension"
        );
    }
    let events=send_and_wait(&app,session,"<customer-input channel=\"mock-official-chat\">我是刚才电话里的林先生，我已经通过测试平台确认关联，想约人工演示。请按之前的需求和预算提交评估。</customer-input>").await;
    let state = phone.lock().unwrap();
    assert_eq!(state.starts, 1, "one physical dial");
    assert_eq!(state.ends, 1, "one explicit hangup");
    assert!(
        state.events.iter().any(|event| event == "call.connected")
            && state.events.iter().any(|event| event == "call.ended"),
        "both lifecycle results were retrieved"
    );
    assert!(
        state.polls > 0 && state.result_reads > 0,
        "real MCP Task protocol"
    );
    let assessment = state
        .assessment
        .as_ref()
        .expect("assessment submitted through MCP");
    assert_eq!(assessment["intent"], "high");
    assert_eq!(assessment["budget"], 50000);
    assert!(assessment["score"].as_f64().unwrap() > 0.75);
    assert!(
        model.connected_inputs.load(Ordering::SeqCst) > 0,
        "connected result reached inference"
    );
    assert_eq!(
        model.result_lookups.load(Ordering::SeqCst),
        0,
        "no redundant result tool round"
    );
    assert!(
        model.text_chunks.load(Ordering::SeqCst) > 1
            && model.tool_chunks.load(Ordering::SeqCst) > 1,
        "text and tool arguments stream"
    );
    assert!(events.iter().any(|e| e["type"] == "agent.message"));
    eprintln!(
        "Zhike passed: text_chunks={} tool_chunks={} dial_count=1",
        model.text_chunks.load(Ordering::SeqCst),
        model.tool_chunks.load(Ordering::SeqCst)
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zhike_mock_mcp_background_result_and_multiturn_sales() {
    // Z1 real platform + deterministic model + MCP Tasks -> direct connected
    // result, streaming text/tool arguments, one call, multi-turn retained
    // budget and qualification. Z2 state-machine forbids pre-invitation submit.
    // This fixture proves text conversation/control, not audio, real carrier,
    // verified cross-platform identity, or Flow Issue acceptance.
    sales_scenario(Arc::new(ScriptedSales), "zhike-fixture", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit DeepSeek credential and network"]
async fn zhike_live_deepseek_mcp_sales() {
    // Z3 same platform/provider fixture as Z1, but a real model chooses tools
    // and conversational replies. Credentials are supplied only to the model
    // adapter; no request, trace or failure assertion prints their value.
    let key = std::env::var("DEEPSEEK_API_KEY").expect("explicit DeepSeek configuration required");
    let model = std::env::var("AWAKEN_GENAI_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
    let executor = awaken_provider_genai::GenaiExecutor::from_materialized_provider_endpoint(
        "deepseek",
        awaken_provider_genai::AdapterKind::OpenAI,
        "https://api.deepseek.com/v1",
        key,
    )
    .unwrap();
    sales_scenario(Arc::new(executor), &model, false).await;
}
