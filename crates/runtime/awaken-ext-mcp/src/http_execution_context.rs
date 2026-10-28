//! Correlation from the runtime's task-local tool invocation, never model args.
//! The receiver must authenticate the caller and resolve the live execution;
//! this header alone confers no credential or Session authority.

pub(super) fn is_runtime_identity_header(name: &str) -> bool {
    [
        "x-awaken-run-id",
        "x-awaken-agent-id",
        "x-awaken-session-id",
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

pub(super) fn with_runtime_execution_header(
    request: reqwest::RequestBuilder,
    endpoint: &str,
) -> reqwest::RequestBuilder {
    if !awaken_mcp_wire::http::execution_context_enabled(endpoint) {
        return request;
    }
    match awaken_tool_contract::tool::current_tool_operation_context()
        .and_then(|context| context.run_id)
    {
        Some(run) if !run.0.trim().is_empty() => request.header("x-awaken-run-id", run.0),
        _ => request,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_tool_contract::tool::{ToolOperationContext, with_tool_operation_context};

    fn request() -> reqwest::Request {
        with_runtime_execution_header(
            reqwest::Client::new().post("http://127.0.0.1/mcp"),
            "http://127.0.0.1/mcp?awaken_execution_context=run-agent-v1",
        )
        .build()
        .unwrap()
    }

    #[tokio::test]
    async fn ordinary_external_mcp_receives_no_execution_identifiers() {
        let result = with_tool_operation_context(
            ToolOperationContext::for_run("private-run", "op"),
            async {
                with_runtime_execution_header(
                    reqwest::Client::new().post("https://external.invalid/mcp"),
                    "https://external.invalid/mcp",
                )
                .build()
                .unwrap()
            },
        )
        .await;
        assert!(result.headers().get("x-awaken-run-id").is_none());
    }

    #[tokio::test]
    async fn runtime_run_is_scoped_to_each_invocation() {
        assert!(request().headers().get("x-awaken-run-id").is_none());
        let (a, b) = tokio::join!(
            with_tool_operation_context(ToolOperationContext::for_run("run-a", "op-a"), async {
                tokio::task::yield_now().await;
                request()
            }),
            with_tool_operation_context(ToolOperationContext::for_run("run-b", "op-b"), async {
                tokio::task::yield_now().await;
                request()
            })
        );
        assert_eq!(a.headers()["x-awaken-run-id"], "run-a");
        assert_eq!(b.headers()["x-awaken-run-id"], "run-b");
        assert!(request().headers().get("x-awaken-run-id").is_none());
    }

    #[tokio::test]
    async fn empty_runtime_identity_is_not_forwarded() {
        let request = with_tool_operation_context(ToolOperationContext::for_run("", "op"), async {
            request()
        })
        .await;
        assert!(request.headers().get("x-awaken-run-id").is_none());
    }

    #[tokio::test]
    async fn static_headers_cannot_supply_or_override_runtime_identity() {
        let transport = super::super::HttpTransportBuilder::new(
            "http://127.0.0.1/mcp?awaken_execution_context=run-agent-v1",
        )
        .header("X-Awaken-Run-Id", "stale-config")
        .build();
        let build = || {
            transport
                .shared
                .post_builder(&serde_json::json!({}), std::time::Duration::from_secs(1))
                .build()
                .unwrap()
        };
        assert!(build().headers().get("x-awaken-run-id").is_none());
        let request =
            with_tool_operation_context(ToolOperationContext::for_run("live-run", "op"), async {
                build()
            })
            .await;
        assert_eq!(request.headers()["x-awaken-run-id"], "live-run");
    }

    #[tokio::test]
    async fn custom_auth_cannot_impersonate_execution_metadata() {
        for name in [
            "X-Awaken-Run-Id",
            "X-Awaken-Agent-Id",
            "X-Awaken-Session-Id",
        ] {
            let transport = super::super::HttpTransportBuilder::new(
                "http://127.0.0.1/mcp?awaken_execution_context=run-agent-v1",
            )
            .header("x-awaken-agent-id", "stale-agent")
            .header("x-awaken-session-id", "stale-session")
            .credential(crate::Credential::Header {
                name: name.into(),
                value: "stale-config".into(),
            })
            .build();
            let build = || {
                transport
                    .shared
                    .post_builder(&serde_json::json!({}), std::time::Duration::from_secs(1))
                    .build()
                    .unwrap()
            };
            let absent = build();
            for name in [
                "x-awaken-run-id",
                "x-awaken-agent-id",
                "x-awaken-session-id",
            ] {
                assert!(absent.headers().get(name).is_none());
            }
            let current = with_tool_operation_context(
                ToolOperationContext::for_run("live-run", "op"),
                async { build() },
            )
            .await;
            assert_eq!(current.headers()["x-awaken-run-id"], "live-run");
            assert_eq!(
                current.headers().get_all("x-awaken-run-id").iter().count(),
                1
            );
            assert!(current.headers().get("x-awaken-agent-id").is_none());
            assert!(current.headers().get("x-awaken-session-id").is_none());
        }
    }
}
