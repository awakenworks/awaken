//! Actual Windows descendants, observed over their own retained connection.
use super::*;
use std::io::{Read, Write};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ADDRESS: &str = "AWAKEN_BASH_DESCENDANT_ADDRESS";
const TOKEN: &str = "AWAKEN_BASH_DESCENDANT_TOKEN";

#[test]
#[ignore = "private child fixture launched only by the Windows ownership tests"]
fn descendant_holds_its_connection_until_terminated() {
    let address: std::net::SocketAddr = std::env::var(ADDRESS)
        .expect("private descendant address")
        .parse()
        .unwrap();
    let token = std::env::var(TOKEN).expect("private descendant token");
    assert_eq!(token.len(), 32);
    let mut stream =
        std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream.write_all(token.as_bytes()).unwrap();
    let mut byte = [0];
    // No timer closes this connection. EOF before the parent's explicit
    // teardown is evidence that the OS terminated this actual descendant.
    while stream.read(&mut byte).unwrap() != 0 {
        stream.write_all(&byte).unwrap();
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct Descendant {
    listener: TcpListener,
    token: String,
}

impl Descendant {
    async fn new() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            token: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    fn command(&self, wait: bool) -> String {
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        format!(
            "{ADDRESS}={} {TOKEN}={} {} --ignored --exact windows_process::descendant_holds_its_connection_until_terminated --quiet >/dev/null 2>&1 &{}",
            quote(&self.listener.local_addr().unwrap().to_string()),
            quote(&self.token),
            quote(&executable),
            if wait { " wait" } else { "" },
        )
    }

    async fn ready(&self) -> TcpStream {
        let (mut connection, _) =
            tokio::time::timeout(Duration::from_secs(5), self.listener.accept())
                .await
                .expect("the actual descendant must connect")
                .unwrap();
        let mut token = [0; 32];
        tokio::time::timeout(Duration::from_secs(2), connection.read_exact(&mut token))
            .await
            .expect("the descendant must publish readiness")
            .unwrap();
        assert_eq!(&token, self.token.as_bytes());
        connection
    }
}

async fn stopped(connection: &mut TcpStream) -> bool {
    let mut byte = [0];
    match tokio::time::timeout(Duration::from_secs(2), connection.read(&mut byte)).await {
        Ok(Ok(0)) => true,
        Ok(Err(error)) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    }
}

async fn prime(tool: &Arc<dyn RawTool>) {
    tool.invoke(call(
        "bash",
        serde_json::json!({
            "command": "export SHOULD_DISAPPEAR=yes; printf ready", "timeout_ms": 1000
        }),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_call_stops_ready_descendants_before_session_reuse() {
    // C1 a real native descendant has authenticated readiness; C2 the call is
    // cancelled while its Bash session remains owned; C3 another session is
    // independently alive. E1 C2 closes C1 before any reuse; E2 C3 still echoes;
    // E3 the next subject call has clean shell state. No PID lookup or sleep.
    let subject_root = tempfile::tempdir().unwrap();
    let peer_root = tempfile::tempdir().unwrap();
    let subject = tool_at("bash", subject_root.path());
    let peer = tool_at("bash", peer_root.path());
    prime(&subject).await;
    let peer_child = Descendant::new().await;
    peer.invoke(call(
        "bash",
        serde_json::json!({ "command": peer_child.command(false), "timeout_ms": 1000 }),
    ))
    .await
    .unwrap();
    let mut peer_connection = peer_child.ready().await;
    let child = Descendant::new().await;
    let running = subject.clone();
    let command = child.command(true);
    let task = tokio::spawn(async move {
        running
            .invoke(call("bash", serde_json::json!({ "command": command })))
            .await
    });
    let mut connection = child.ready().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let terminated = stopped(&mut connection).await;
    peer_connection.write_all(b"p").await.unwrap();
    let mut echo = [0];
    tokio::time::timeout(
        Duration::from_secs(2),
        peer_connection.read_exact(&mut echo),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(echo, *b"p", "another process tree must remain alive");
    // Closing the fixture channel also lets an unfixed descendant exit, so a
    // failing red run does not leave a permanent helper behind.
    drop(connection);
    drop(peer_connection);
    peer.invoke(call("bash", serde_json::json!({ "restart": true })))
        .await
        .unwrap();
    let clean = subject
        .invoke(call(
            "bash",
            serde_json::json!({ "command": "printf '%s' \"$SHOULD_DISAPPEAR\"" }),
        ))
        .await
        .unwrap();
    assert_eq!(clean.text(), "");
    assert!(
        terminated,
        "cancelled Bash left an authenticated descendant alive before reuse"
    );
}

#[tokio::test]
async fn timed_out_call_stops_ready_descendants_before_returning() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool_at("bash", root.path());
    prime(&tool).await;
    let child = Descendant::new().await;
    let running = tool.clone();
    let command = child.command(true);
    let task = tokio::spawn(async move {
        running
            .invoke(call(
                "bash",
                serde_json::json!({ "command": command, "timeout_ms": 1000 }),
            ))
            .await
    });
    let mut connection = child.ready().await;
    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("timed out after 1000ms"));
    let terminated = stopped(&mut connection).await;
    drop(connection);
    let clean = tool
        .invoke(call(
            "bash",
            serde_json::json!({ "command": "printf '%s' \"$SHOULD_DISAPPEAR\"" }),
        ))
        .await
        .unwrap();
    assert_eq!(clean.text(), "");
    assert!(
        terminated,
        "timed-out Bash left an authenticated descendant alive"
    );
}

#[tokio::test]
async fn restarting_the_session_stops_ready_background_descendants() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool_at("bash", root.path());
    prime(&tool).await;
    let child = Descendant::new().await;
    tool.invoke(call(
        "bash",
        serde_json::json!({ "command": child.command(false), "timeout_ms": 1000 }),
    ))
    .await
    .unwrap();
    let mut connection = child.ready().await;
    tool.invoke(call("bash", serde_json::json!({ "restart": true })))
        .await
        .unwrap();
    let terminated = stopped(&mut connection).await;
    drop(connection);
    let next = tool
        .invoke(call(
            "bash",
            serde_json::json!({ "command": "printf '%s' \"$SHOULD_DISAPPEAR\"" }),
        ))
        .await
        .unwrap();
    assert_eq!(next.text(), "");
    assert!(
        terminated,
        "restarting Bash left an authenticated descendant alive"
    );
}

#[tokio::test]
async fn dropping_the_session_stops_ready_background_descendants() {
    let root = tempfile::tempdir().unwrap();
    let tool = tool_at("bash", root.path());
    let child = Descendant::new().await;
    tool.invoke(call(
        "bash",
        serde_json::json!({ "command": child.command(false), "timeout_ms": 1000 }),
    ))
    .await
    .unwrap();
    let mut connection = child.ready().await;
    drop(tool);
    let terminated = stopped(&mut connection).await;
    drop(connection);
    assert!(
        terminated,
        "dropping Bash left an authenticated background descendant alive"
    );
}
