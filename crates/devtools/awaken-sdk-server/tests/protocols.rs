//! Exercise the independent SDK process through each mounted wire protocol.
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{Value, json};

struct Server {
    child: Child,
    address: String,
    client: Client,
}

impl Drop for Server {
    fn drop(&mut self) {
        // This process owns no child commands. Reap exactly the child we started.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start() -> Self {
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        let executable = std::env::var("CARGO_BIN_EXE_awaken-sdk-server")
            .or_else(|_| std::env::var("NEXTEST_BIN_EXE_awaken-sdk-server"))
            .expect("Cargo must provide the SDK server path when this integration test runs");
        let child = Command::new(executable)
            .arg("127.0.0.1:0")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            address: String::new(),
            client,
        };
        let stdout = server.child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        // Readiness is emitted only after the actual ephemeral listener binds.
        let line = rx.recv_timeout(Duration::from_secs(20)).unwrap().unwrap();
        reader.join().unwrap();
        let address = line
            .trim()
            .strip_prefix("SDK protocols listening on ")
            .expect("the server must announce its bound listener");
        let address: std::net::SocketAddr = address.parse().unwrap();
        assert!(address.ip().is_loopback());
        assert_ne!(address.port(), 0);
        server.address = format!("http://{address}");
        server
    }

    fn post(&self, path: &str, input: Value) -> String {
        let response = self
            .client
            .post(format!("{}{path}", self.address))
            .header("accept", "application/json, text/event-stream")
            .json(&input)
            .send()
            .unwrap();
        let status = response.status();
        let body = response.text().unwrap();
        assert!(status.is_success(), "{path}: {status} {body}");
        body
    }
}

#[test]
fn independent_sdk_server_serves_all_four_protocols() {
    let server = Server::start();
    let ag_ui = server.post(
        "/v1/ag-ui",
        json!({"threadId":"ag", "runId":"ag-1", "messages":[
            {"id":"u1", "role":"user", "content":"native ag-ui"}
        ]}),
    );
    assert!(ag_ui.contains("Echo: native ag-ui"), "{ag_ui}");
    assert!(ag_ui.contains("RUN_FINISHED"), "{ag_ui}");
    let ai_sdk = server.post(
        "/v1/ai-sdk/chat",
        json!({"threadId":"ai", "messages":[
            {"id":"u2", "role":"user", "parts":[{"type":"text", "text":"native ai-sdk"}]}
        ]}),
    );
    assert!(ai_sdk.contains("Echo: native ai-sdk"), "{ai_sdk}");
    assert!(ai_sdk.contains("finish"), "{ai_sdk}");
    let a2a = server.post(
        "/v1/a2a/message:send",
        json!({"message":{"kind":"message", "messageId":"a2a-1", "contextId":"a2a",
            "role":"user", "parts":[{"kind":"text", "text":"native a2a"}]}}),
    );
    let a2a: Value = serde_json::from_str(&a2a).unwrap();
    assert_eq!(a2a["task"]["status"]["state"], "completed", "{a2a}");
    assert!(a2a.to_string().contains("Echo: native a2a"), "{a2a}");
    let initialized = server.post(
        "/mcp",
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"sdk-process-test", "version":"1"}}}),
    );
    let initialized: Value = serde_json::from_str(&initialized).unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    let tools = server.post(
        "/mcp",
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/list", "params":{}}),
    );
    let tools: Value = serde_json::from_str(&tools).unwrap();
    assert_eq!(tools["result"]["tools"], json!([]));
}
