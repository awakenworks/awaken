//! Session tool export port and its opaque lifetime lease.
//! Hosts own projection timing; MCP adapters implement transport assembly.

use std::sync::Arc;

pub struct AcpToolExport {
    pub server: awaken_runtime_contract::resolved::AcpMcpServer,
    _lease: Box<dyn Send + Sync>,
}

impl AcpToolExport {
    #[must_use]
    pub fn new(
        server: awaken_runtime_contract::resolved::AcpMcpServer,
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
    /// The attached-channel capability of this exporter, if it has one.
    fn attached_channels(&self) -> Option<&dyn AttachedChannelToolExporter> {
        None
    }

    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    ) -> Result<AcpToolExport, String>;

    async fn export(
        &self,
        server_name: &str,
        descriptor: awaken_runtime_contract::resolved::ToolDescriptor,
        tool: Arc<dyn awaken_tool_contract::tool::RawTool>,
    ) -> Result<AcpToolExport, String> {
        self.export_set(server_name, vec![descriptor], vec![tool])
            .await
    }
}

/// One validated Session tool set for an attached ACP channel. The descriptor
/// and executor identities are paired once before either process is launched.
pub struct AttachedToolSet {
    exporter: Arc<dyn AcpToolExporter>,
    descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
    executors: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
}

impl AttachedToolSet {
    pub fn new(
        exporter: Arc<dyn AcpToolExporter>,
        descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        executors: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    ) -> Result<Self, String> {
        let (descriptors, executors) = pair_tools(descriptors, executors)?;
        Ok(Self {
            exporter,
            descriptors,
            executors,
        })
    }

    #[must_use]
    pub fn exporter(&self) -> &dyn AcpToolExporter {
        self.exporter.as_ref()
    }

    #[must_use]
    pub fn descriptors(&self) -> &[awaken_runtime_contract::resolved::ToolDescriptor] {
        &self.descriptors
    }

    #[must_use]
    pub fn executors(&self) -> &[Arc<dyn awaken_tool_contract::tool::RawTool>] {
        &self.executors
    }
}

/// The one descriptor/executor identity and ordering check used by both
/// process-local and attached ACP MCP exports.
type PairedTools = (
    Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
    Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
);

pub fn pair_tools(
    descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
    executors: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
) -> Result<PairedTools, String> {
    let descriptor_ids = descriptors
        .iter()
        .map(|descriptor| descriptor.id.clone())
        .collect::<Vec<_>>();
    let executor_ids = executors
        .iter()
        .map(|tool| tool.id().to_string())
        .collect::<Vec<_>>();
    if descriptor_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != descriptor_ids.len()
    {
        return Err(format!(
            "duplicate ACP tool descriptor id: descriptors={descriptor_ids:?}"
        ));
    }
    let executors = executors
        .into_iter()
        .map(|tool| (tool.id().to_string(), tool))
        .collect::<std::collections::BTreeMap<_, _>>();
    if descriptors.len() != executors.len() {
        return Err(format!(
            "ACP tool descriptor/executor sets differ: descriptors={descriptor_ids:?}, executors={executor_ids:?}"
        ));
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
    Ok((paired_descriptors, paired_executors))
}

/// Optional capability of an [`AcpToolExporter`] that serves tools on an
/// attached process channel (ADR-0080 capability ladder).
#[async_trait::async_trait]
pub trait AttachedChannelToolExporter: AcpToolExporter {
    /// Serve the same tool set on the multiplexed attached process channel.
    /// Each local client owns an independent MCP peer; the export lease owns
    /// all peers. The Host supplies the workload's exact stdio command, the
    /// connection adapter owns byte substreams, and MCP owns their framing.
    async fn export_channel_set(
        &self,
        server: awaken_runtime_contract::resolved::AcpMcpServer,
        descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ) -> Result<AcpToolExport, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestTool(&'static str);

    #[async_trait::async_trait]
    impl awaken_tool_contract::tool::RawTool for TestTool {
        fn id(&self) -> &str {
            self.0
        }

        async fn invoke(
            &self,
            _: awaken_tool_contract::tool::ToolCall,
        ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError>
        {
            unreachable!("identity admission never invokes a tool")
        }
    }

    struct TestExporter;

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct TestArgs {
        value: String,
    }

    #[async_trait::async_trait]
    impl AcpToolExporter for TestExporter {
        async fn export_set(
            &self,
            _: &str,
            _: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
            _: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
        ) -> Result<AcpToolExport, String> {
            unreachable!("identity admission never opens an export")
        }
    }

    fn descriptor(id: &str) -> awaken_runtime_contract::resolved::ToolDescriptor {
        awaken_runtime_contract::resolved::ToolDescriptor::for_args::<TestArgs>("test", id, id)
    }

    fn tool(id: &'static str) -> Arc<dyn awaken_tool_contract::tool::RawTool> {
        Arc::new(TestTool(id))
    }

    #[test]
    fn attached_tool_set_requires_one_exact_descriptor_executor_bijection() {
        // Causes: C1 descriptor and executor IDs form the same unique set;
        // C2 caller order differs; C3 a descriptor repeats while one executor
        // is extra; C4 an executor repeats; C5 an identity is missing.
        // Effects: E1 a validated set follows descriptor order; E2 all other
        // cases fail before any export or process. Constraint: no unchecked
        // constructor may create an attached set. Decision table: R1 C1 -> E1;
        // R2 C1+C2 -> E1; R3 C3 -> E2; R4 C4 -> E2; R5 C5 -> E2.
        let set = AttachedToolSet::new(
            Arc::new(TestExporter),
            vec![descriptor("a"), descriptor("b")],
            vec![tool("b"), tool("a")],
        )
        .expect("R2 reorders the unique executor set");
        assert_eq!(
            set.descriptors()
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            set.executors()
                .iter()
                .map(|tool| tool.id())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        for (rule, descriptors, executors) in [
            (
                "R3",
                vec![descriptor("a"), descriptor("a")],
                vec![tool("a"), tool("b")],
            ),
            (
                "R4",
                vec![descriptor("a"), descriptor("b")],
                vec![tool("a"), tool("a")],
            ),
            (
                "R5",
                vec![descriptor("a"), descriptor("b")],
                vec![tool("a")],
            ),
        ] {
            assert!(
                AttachedToolSet::new(Arc::new(TestExporter), descriptors, executors).is_err(),
                "{rule}"
            );
        }
    }
}
