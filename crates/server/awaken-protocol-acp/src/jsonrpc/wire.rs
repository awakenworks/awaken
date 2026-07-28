//! Newline-delimited JSON-RPC transport over one ACP agent channel.

use awaken_agent_channel::AgentChannel;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::AcpError;

pub(super) const JSONRPC: &str = "2.0";

#[derive(Serialize)]
struct OutRequest<'a, P: Serialize> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: P,
}

/// Parsed inbound response, request, or notification.
#[derive(serde::Deserialize)]
pub(super) struct Incoming {
    #[serde(default)]
    pub(super) id: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) method: Option<String>,
    #[serde(default)]
    pub(super) params: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) result: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) error: Option<serde_json::Value>,
}

/// One-message-per-line transport used by the official ACP stdio connection.
pub(super) struct Wire<'a> {
    reader: BufReader<&'a mut dyn AgentChannel>,
    line: String,
}

impl<'a> Wire<'a> {
    pub(super) fn new(channel: &'a mut dyn AgentChannel) -> Self {
        Self {
            reader: BufReader::new(channel),
            line: String::new(),
        }
    }

    pub(super) async fn send<P: Serialize>(&mut self, msg: &P) -> Result<(), AcpError> {
        let mut buf =
            serde_json::to_vec(msg).map_err(|error| AcpError::Frame(error.to_string()))?;
        buf.push(b'\n');
        self.reader
            .get_mut()
            .write_all(&buf)
            .await
            .map_err(|error| AcpError::Io(error.to_string()))?;
        self.reader
            .get_mut()
            .flush()
            .await
            .map_err(|error| AcpError::Io(error.to_string()))
    }

    pub(super) async fn send_request<P: Serialize>(
        &mut self,
        id: u64,
        method: &str,
        params: P,
    ) -> Result<(), AcpError> {
        self.send(&OutRequest {
            jsonrpc: JSONRPC,
            id,
            method,
            params,
        })
        .await
    }

    pub(super) async fn read(&mut self) -> Result<Option<Incoming>, AcpError> {
        loop {
            self.line.clear();
            let count = self
                .reader
                .read_line(&mut self.line)
                .await
                .map_err(|error| AcpError::Io(error.to_string()))?;
            if count == 0 {
                return Ok(None);
            }
            let line = self.line.trim();
            if line.is_empty() {
                continue;
            }
            return serde_json::from_str::<Incoming>(line)
                .map(Some)
                .map_err(|error| AcpError::Frame(error.to_string()));
        }
    }
}
