//! Windows qualification: actual ACP executor/JSON-RPC/owned child process,
//! explicit loopback application MCP, synthetic caller and no model service.
use awaken_run_executor_acp::{
    AcpLaunch, AcpRunExecutor, Codec, SessionMcpServer, SubprocessChannelSource,
};
use awaken_runtime_contract::{execution::RunExecutor, runtime_context::RuntimeRunContext};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    node: String,
    peer: String,
    endpoint: String,
    token: String,
    run_id: String,
    session_id: String,
    agent_id: String,
    tool: String,
    arguments: serde_json::Value,
}

struct LoginPolicy;
#[async_trait::async_trait]
impl awaken_runtime_contract::permission::ToolPermissionPolicy for LoginPolicy {
    async fn evaluate(
        &self,
        call: &awaken_tool_contract::tool::ToolCall,
    ) -> awaken_runtime_contract::permission::ToolPermissionRuling {
        use awaken_runtime_contract::permission::{
            PermissionBasis, ToolPermissionRuling, ToolPermissionVerdict,
        };
        let allowed = [
            "mcp__application__login_start",
            "mcp__application__login_status",
            "mcp__application__login_verify",
            "mcp__application__login_cancel",
        ]
        .contains(&call.tool_id.as_str());
        ToolPermissionRuling::policy(
            if allowed {
                ToolPermissionVerdict::Allow
            } else {
                ToolPermissionVerdict::Deny {
                    reason: "qualification login surface only".into(),
                }
            },
            PermissionBasis::fixed(
                awaken_agent_contract::audit::permission::PermissionPolicy::Rule,
            ),
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(windows) {
        return Err("Windows only".into());
    }
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("private request file required")?,
    );
    let request: Request = serde_json::from_slice(&std::fs::read(&path)?)?;
    let endpoint = url::Url::parse(&request.endpoint)?;
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.path() != "/mcp"
    {
        return Err("qualification loopback endpoint required".into());
    }
    if std::env::args().nth(2).as_deref() == Some("--native-mcp") {
        return native_mcp(&request, &path).await;
    }
    let source = Arc::new(
        SubprocessChannelSource::new(AcpLaunch::custom(
            vec![request.node, request.peer, path.to_string_lossy().into()],
            Vec::new(),
        ))
        .with_codec(Codec::Acp),
    );
    let executor = AcpRunExecutor::new(source).for_session_servers(
        Arc::new(LoginPolicy),
        &[SessionMcpServer {
            name: "application".into(),
            command: None,
            args: Vec::new(),
            url: Some(request.endpoint),
            auth: Some(("Authorization".into(), format!("Bearer {}", request.token))),
        }],
    );
    let mut activation = awaken_dispatch_testkit::worker_http::activation("a");
    activation.run_id = awaken_agent_contract::agent::run::Id(request.run_id);
    activation.thread_id = awaken_agent_contract::agent::thread::Id(request.session_id);
    activation.snapshot.root_agent_id =
        awaken_runtime_contract::snapshot::AgentId(request.agent_id);
    activation.snapshot.resolved_spec.max_steps = Some(1);
    activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::parse(
                "qualification",
                "fixture",
                "acp:codex",
            )?,
        );
    let commits = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
        awaken_time::system_clock(),
    ));
    let state = executor
        .execute(
            activation,
            RuntimeRunContext::new().with_commit(commits.clone()),
        )
        .await?;
    if !matches!(
        state,
        awaken_agent_contract::agent::run::RunState::Ended(
            awaken_agent_contract::agent::run::EndCause::NaturalEnd
        )
    ) {
        std::fs::write(path.with_extension("state.txt"), format!("{state:?}"))?;
        return Err("ACP qualification did not complete".into());
    }
    if commits.commit_count() == 0 {
        return Err("ACP produced no canonical commit".into());
    }
    Ok(())
}

// The native path uses the production HTTP transport and runtime-owned
// task-local context. It does not manufacture headers in the application test.
async fn native_mcp(
    request: &Request,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use awaken_ext_mcp::{HttpTransportBuilder, McpToolTransport};
    use awaken_tool_contract::tool::{ToolOperationContext, with_tool_operation_context};
    use serde_json::json;
    if ![
        "login_start",
        "login_status",
        "login_verify",
        "login_cancel",
    ]
    .contains(&request.tool.as_str())
    {
        return Err("qualification login surface only".into());
    }
    let output = with_tool_operation_context(
        ToolOperationContext::for_run(request.run_id.clone(), "native-login-qualification"),
        async {
            let transport = HttpTransportBuilder::new(&request.endpoint)
                .header("Authorization", format!("Bearer {}", request.token))
                .connect()
                .await?;
            transport
                .call_tool(&request.tool, request.arguments.clone())
                .await
        },
    )
    .await?;
    let result = json!({"jsonrpc":"2.0","id":1,"result":output});
    let context = if awaken_mcp_wire::http::execution_context_enabled(&request.endpoint) {
        json!({"x-awaken-run-id":request.run_id})
    } else {
        json!({})
    };
    let call =
        json!({"method":"tools/call","params":{"name":request.tool,"arguments":request.arguments}});
    std::fs::write(
        format!("{}.result.json", path.display()),
        serde_json::to_vec(&json!({
            "result":result,"context":context,"model_tape":{"request":call,"result":result}
        }))?,
    )?;
    Ok(())
}
