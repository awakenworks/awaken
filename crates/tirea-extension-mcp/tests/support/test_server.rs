#![allow(dead_code)]

use async_trait::async_trait;
use mcp::transport::{McpTransportError, TransportTypeId};
use mcp::{CallToolResult, McpToolDefinition, ToolContent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tirea_extension_mcp::{McpProgressUpdate, McpToolTransport};
use tokio::sync::mpsc;

pub struct ResourceContent {
    pub text: String,
    pub mime_type: String,
}

type CallHandler = dyn Fn(&str, &Value) -> Result<CallToolResult, String> + Send + Sync;

pub struct TestMcpTransport {
    tools: Vec<McpToolDefinition>,
    resources: HashMap<String, ResourceContent>,
    call_handler: Option<Box<CallHandler>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl TestMcpTransport {
    pub fn new(tools: Vec<McpToolDefinition>) -> Self {
        Self {
            tools,
            resources: HashMap::new(),
            call_handler: None,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_resource(
        mut self,
        uri: impl Into<String>,
        text: impl Into<String>,
        mime_type: impl Into<String>,
    ) -> Self {
        self.resources.insert(
            uri.into(),
            ResourceContent {
                text: text.into(),
                mime_type: mime_type.into(),
            },
        );
        self
    }

    pub fn with_call_handler(
        mut self,
        handler: impl Fn(&str, &Value) -> Result<CallToolResult, String> + Send + Sync + 'static,
    ) -> Self {
        self.call_handler = Some(Box::new(handler));
        self
    }

    #[allow(dead_code)]
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl McpToolTransport for TestMcpTransport {
    async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
        Ok(self.tools.clone())
    }

    async fn call_tool(
        &self,
        name: &str,
        args: Value,
        _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
    ) -> Result<CallToolResult, McpTransportError> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_string(), args.clone()));
        if let Some(ref handler) = self.call_handler {
            handler(name, &args).map_err(McpTransportError::ServerError)
        } else {
            Ok(CallToolResult {
                content: vec![ToolContent::text("ok")],
                structured_content: Some(json!({"result": "ok"})),
                is_error: None,
            })
        }
    }

    fn transport_type(&self) -> TransportTypeId {
        TransportTypeId::Stdio
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, McpTransportError> {
        if let Some(resource) = self.resources.get(uri) {
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "text": resource.text,
                    "mimeType": resource.mime_type,
                }]
            }))
        } else {
            Err(McpTransportError::ServerError(format!(
                "Resource not found: {}",
                uri
            )))
        }
    }
}
