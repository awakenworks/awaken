//! Neutral Host port for exporting a Session-owned tool to an ACP workload.
//!
//! The Host owns when a tool is projected and how long it lives. A higher
//! protocol adapter owns the concrete MCP server transport.

use std::sync::Arc;

pub struct AcpToolExport {
    pub server: awaken_run_executor_acp::McpServerConfig,
    _lease: Box<dyn Send + Sync>,
}

impl AcpToolExport {
    #[must_use]
    pub fn new(
        server: awaken_run_executor_acp::McpServerConfig,
        lease: impl Send + Sync + 'static,
    ) -> Self {
        Self {
            server,
            _lease: Box::new(lease),
        }
    }
}

#[async_trait::async_trait]
pub trait AcpToolExporter: Send + Sync {
    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Result<AcpToolExport, String>;

    async fn export(
        &self,
        server_name: &str,
        descriptor: awaken_runtime_contract::resolved::ToolDescriptor,
        tool: Arc<dyn awaken_runtime_contract::tool::RawTool>,
    ) -> Result<AcpToolExport, String> {
        self.export_set(server_name, vec![descriptor], vec![tool])
            .await
    }
}

/// Export one exact descriptor/executor set through the existing ACP MCP port.
/// The complete set is served by one Session MCP endpoint and one lifetime
/// lease. Native and ACP therefore consume identical contracts without turning
/// each semantic tool into a separate server/process lifecycle.
pub(crate) async fn export_tools(
    exporter: &dyn AcpToolExporter,
    namespace: &str,
    descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
    executors: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
) -> Result<
    (
        Vec<awaken_run_executor_acp::SessionMcpServer>,
        Vec<AcpToolExport>,
    ),
    String,
> {
    let executors = executors
        .into_iter()
        .map(|tool| (tool.id().to_string(), tool))
        .collect::<std::collections::BTreeMap<_, _>>();
    if descriptors.len() != executors.len() {
        return Err("ACP tool descriptor/executor sets differ in size".into());
    }
    let mut paired_descriptors = Vec::with_capacity(descriptors.len());
    let mut paired_executors = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let tool = executors.get(&descriptor.id).cloned().ok_or_else(|| {
            format!(
                "ACP tool descriptor `{}` has no matching executor",
                descriptor.id
            )
        })?;
        paired_descriptors.push(descriptor);
        paired_executors.push(tool);
    }
    let export = exporter
        .export_set(namespace, paired_descriptors, paired_executors)
        .await?;
    let server = match export.server.transport.clone() {
        awaken_runtime_contract::resolved::AcpMcpTransport::Stdio { command, args } => {
            awaken_run_executor_acp::SessionMcpServer {
                name: export.server.name.clone(),
                command: Some(command),
                args,
                url: None,
                auth: None,
            }
        }
        awaken_runtime_contract::resolved::AcpMcpTransport::Http { url } => {
            awaken_run_executor_acp::SessionMcpServer {
                name: export.server.name.clone(),
                command: None,
                args: Vec::new(),
                url: Some(url),
                auth: None,
            }
        }
    };
    Ok((vec![server], vec![export]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingExporter(Mutex<Vec<(String, Vec<String>)>>);

    #[async_trait::async_trait]
    impl AcpToolExporter for RecordingExporter {
        async fn export_set(
            &self,
            server_name: &str,
            descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
            _tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
        ) -> Result<AcpToolExport, String> {
            self.0.lock().unwrap().push((
                server_name.into(),
                descriptors
                    .into_iter()
                    .map(|descriptor| descriptor.id)
                    .collect(),
            ));
            Ok(AcpToolExport::new(
                awaken_run_executor_acp::McpServerConfig {
                    name: server_name.into(),
                    transport: awaken_run_executor_acp::McpTransport::Http {
                        url: format!("http://127.0.0.1/{server_name}"),
                    },
                },
                (),
            ))
        }
    }

    struct Echo(&'static str);

    #[async_trait::async_trait]
    impl awaken_runtime_contract::tool::RawTool for Echo {
        fn id(&self) -> &str {
            self.0
        }

        async fn invoke(
            &self,
            call: awaken_runtime_contract::tool::ToolCall,
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
            Ok(awaken_runtime_contract::tool::ToolOutput::ok(
                call.call_id,
                self.0,
            ))
        }
    }

    fn descriptor(id: &str) -> awaken_runtime_contract::resolved::ToolDescriptor {
        awaken_runtime_contract::resolved::ToolDescriptor::pinned(
            "test",
            id,
            id,
            serde_json::json!({"type": "object"}),
        )
    }

    #[tokio::test]
    async fn acp_export_reuses_the_exact_native_descriptor_executor_set() {
        // Cause/effect decision table: R1 exact two-tool set -> two deterministic
        // MCP routes and two retained leases; R2 missing executor -> reject before
        // exporting anything. This is the ACP/native compatibility invariant:
        // only transport changes, never the semantic tool contracts.
        let exporter = RecordingExporter::default();
        let (servers, leases) = export_tools(
            &exporter,
            "awaken_session",
            vec![descriptor("list_memories"), descriptor("read_memory")],
            vec![
                Arc::new(Echo("list_memories")),
                Arc::new(Echo("read_memory")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(servers.len(), 1, "R1 one route");
        assert_eq!(leases.len(), 1, "R1 one lease");
        assert_eq!(
            exporter.0.lock().unwrap().as_slice(),
            &[(
                "awaken_session".into(),
                vec!["list_memories".into(), "read_memory".into()]
            )],
            "R1 exact contracts"
        );

        let missing = RecordingExporter::default();
        assert!(
            export_tools(
                &missing,
                "awaken_session",
                vec![descriptor("read_memory")],
                Vec::new(),
            )
            .await
            .is_err(),
            "R2"
        );
        assert!(missing.0.lock().unwrap().is_empty(), "R2 no side effect");
    }
}
