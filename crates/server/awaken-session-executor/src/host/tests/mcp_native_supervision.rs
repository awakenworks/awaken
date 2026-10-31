//! T16-C03: a started native MCP process always has a supervisor. Cancelling
//! the drain waiter keeps the exact owner, the canonical reaper stops the real
//! child, and dropping the Host releases the last owner with no cycle.
//!
//! The child is this test binary re-entered as an ignored participant. It
//! reports readiness over a TCP witness and holds that connection until it is
//! stopped, so witness EOF is the physical exit oracle.

use super::*;
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::mcp::{
    McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
};
use awaken_session_mcp::{McpGenerationProjection, McpProjectionState, McpWiring};
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

const WITNESS_ENV: &str = "AWAKEN_NATIVE_MCP_CHILD_WITNESS";

/// The provisioning-contract shape of a provider process: status, signal, and
/// a last-owner drop that stops the child.
struct ChildProcess {
    id: String,
    child: Mutex<std::process::Child>,
}

impl ChildProcess {
    fn status(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| pc::SandboxError::backend("poll", "child mutex poisoned"))?;
        child
            .try_wait()
            .map(|status| {
                status.map(|status| pc::ExitStatus {
                    code: status.code(),
                    signaled: status.code().is_none(),
                })
            })
            .map_err(|error| pc::SandboxError::backend("poll", error))
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if let Ok(child) = self.child.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[async_trait::async_trait]
impl pc::ProcessHandle for ChildProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        loop {
            if let Some(status) = self.status()? {
                return Ok(status);
            }
            // awaken-allow(wait): std::process::Child has no async exit notification; the reaper bounds this poll.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        self.status()
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        if self.status()?.is_some() {
            return Ok(());
        }
        let mut child = self
            .child
            .lock()
            .map_err(|_| pc::SandboxError::backend("signal", "child mutex poisoned"))?;
        match child.kill() {
            Ok(()) => Ok(()),
            Err(error) => match child.try_wait() {
                Ok(Some(_)) => Ok(()),
                _ => Err(pc::SandboxError::backend("signal", error)),
            },
        }
    }
}

/// A started child observed without owning it, and the witness connection it
/// holds open while alive.
struct StartedChild {
    process: std::sync::Weak<ChildProcess>,
    witness: tokio::net::TcpStream,
}

/// Start one child; the returned `Arc` is its only strong owner.
async fn start_child() -> (Arc<ChildProcess>, StartedChild) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, path)| path);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{module}::native_mcp_child_participant"),
            "--ignored",
            "--nocapture",
        ])
        .env(WITNESS_ENV, &address)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the MCP child participant");
    let process = Arc::new(ChildProcess {
        id: format!("native-mcp-{}", child.id()),
        child: Mutex::new(child),
    });
    let (witness, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
        .await
        .expect("the child reports readiness in time")
        .expect("accept the readiness witness");
    let mut reader = tokio::io::BufReader::new(witness);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
        .await
        .expect("readiness line in time")
        .expect("read readiness");
    assert!(line.starts_with("ready:"), "child readiness: {line:?}");
    let observed = StartedChild {
        process: Arc::downgrade(&process),
        witness: reader.into_inner(),
    };
    (process, observed)
}

/// True when the child's witness connection closed within `within`.
async fn witness_closed(witness: &mut tokio::net::TcpStream, within: Duration) -> bool {
    let mut byte = [0u8; 1];
    match tokio::time::timeout(within, witness.read(&mut byte)).await {
        Ok(Ok(0) | Err(_)) => true,
        Ok(Ok(_)) | Err(_) => false,
    }
}

async fn host_with_process_reaper() -> Arc<ExecutorComponents> {
    (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_mcp_process_reaper(Arc::new(awaken_run_executor_acp::Supervisor)))
    .build()
    .expect("validated Host dependencies")
}

fn projection(
    generation: &McpGenerationRef,
    process: Arc<dyn pc::ProcessHandle>,
    call_fences: Vec<awaken_ext_mcp::transport::McpCallFence>,
) -> McpGenerationProjection {
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realize-native".into(),
        stage_idempotency_key: "stage-native".into(),
        name: "native".into(),
        target: awaken_agent_contract::McpTarget::sandbox_stdio("native-mcp", Vec::new()).unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    McpGenerationProjection {
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: None,
        native_wiring: Some(McpWiring {
            notification_transports: Vec::new(),
            plugins: Vec::new(),
            tool_ids: Vec::new(),
            skill_registries: Vec::new(),
            call_fences,
        }),
        mcp_process: Some(process),
        platform_egress: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Active,
    }
}

struct DropGate(Arc<(Mutex<bool>, Condvar)>);

impl Drop for DropGate {
    fn drop(&mut self) {
        let (released, signal) = self.0.as_ref();
        let mut released = released.lock().expect("drop gate mutex");
        while !*released {
            released = signal.wait(released).expect("drop gate wait");
        }
    }
}

/// An accepted MCP call whose cancellation settles only when the gate opens,
/// so the drain waiter is provably pending before the reaper runs.
struct BusyTransport {
    started: Arc<tokio::sync::Notify>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

#[async_trait::async_trait]
impl awaken_ext_mcp::transport::McpToolTransport for BusyTransport {
    async fn list_tools(
        &self,
    ) -> Result<Vec<awaken_ext_mcp::McpToolDefinition>, awaken_ext_mcp::McpTransportError> {
        Ok(Vec::new())
    }

    async fn call_tool(
        &self,
        _tool_name: &str,
        _arguments: serde_json::Value,
    ) -> Result<awaken_ext_mcp::CallToolResult, awaken_ext_mcp::McpTransportError> {
        let _gate = DropGate(self.gate.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}

/// The supervision oracle: the exact process owner is still installed in a
/// Draining projection and the physical child is alive.
async fn owner_retained(
    host: &ExecutorComponents,
    generation: &McpGenerationRef,
    child: &mut StartedChild,
) -> Result<(), String> {
    let Some(retained) = host.run_assembly.mcp_projection(generation) else {
        return Err("the projection owner was dropped".into());
    };
    if retained.state != McpProjectionState::Draining {
        return Err(format!("projection left Draining: {:?}", retained.state));
    }
    let Some(expected) = child.process.upgrade() else {
        return Err("the process owner was released".into());
    };
    let expected: Arc<dyn pc::ProcessHandle> = expected;
    if !retained
        .mcp_process
        .as_ref()
        .is_some_and(|owner| Arc::ptr_eq(owner, &expected))
    {
        return Err("the exact process owner was lost".into());
    }
    if witness_closed(&mut child.witness, Duration::from_millis(300)).await {
        return Err("the child stopped without a supervisor".into());
    }
    Ok(())
}

/// | Rule | event | Effect |
/// |---|---|---|
/// | S1 (NC fault) | drain waiter aborted while a call settles | Draining, same process owner, child alive |
/// | S2 | Host dropped after S1 | last owner released: child stops, no assembly cycle |
/// | S3 | fresh Host drains a live child | canonical reaper stops it while the Host lives; Removed |
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_mcp_child_is_supervised_across_cancel_drop_and_drain() {
    let host = host_with_process_reaper().await;
    let generation = crate::test_mcp::mcp_fence(
        "mcp-native-supervision",
        "native",
        1,
        "runtime-1",
        1,
        u64::MAX,
    );
    let (owner, mut child) = start_child().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (transport, fence) =
        awaken_ext_mcp::transport::revocable_transport(Arc::new(BusyTransport {
            started: started.clone(),
            gate: gate.clone(),
        }));
    host.run_assembly
        .insert_mcp_projection(projection(&generation, owner, vec![fence]))
        .unwrap();
    let busy =
        tokio::spawn(async move { transport.call_tool("busy", serde_json::Value::Null).await });
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the busy call is accepted");

    let drain_host = host.clone();
    let drain_generation = generation.clone();
    let waiter = tokio::spawn(async move {
        drain_host
            .run_assembly
            .drain_mcp_projection(&drain_generation)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !host
            .run_assembly
            .mcp_projection(&generation)
            .is_some_and(|projection| projection.state == McpProjectionState::Draining)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S1 drain enters Draining");
    waiter.abort();
    let _ = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("S1 the aborted waiter settles");
    owner_retained(&host, &generation, &mut child)
        .await
        .expect("S1 cancelling the waiter keeps the supervisor");

    let (released, signal) = gate.as_ref();
    *released.lock().expect("release gate") = true;
    signal.notify_all();
    let _ = tokio::time::timeout(Duration::from_secs(10), busy)
        .await
        .expect("the cancelled call settles");

    let weak = Arc::downgrade(&host.run_assembly);
    drop(host);
    assert!(
        witness_closed(&mut child.witness, Duration::from_secs(10)).await,
        "S2 the last owner stops the child"
    );
    assert!(weak.upgrade().is_none(), "S2 no assembly cycle");
    assert!(
        child.process.upgrade().is_none(),
        "S2 the Host held the last process owner"
    );

    let host = host_with_process_reaper().await;
    // The test keeps a strong owner here, so only the reaper can stop it.
    let (kept, mut fresh) = start_child().await;
    host.run_assembly
        .insert_mcp_projection(projection(&generation, kept.clone(), Vec::new()))
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        host.run_assembly.drain_mcp_projection(&generation),
    )
    .await
    .expect("S3 drain completes in time")
    .expect("S3 the reaper stops the child");
    assert_eq!(
        host.run_assembly
            .mcp_projection(&generation)
            .expect("S3 tombstone")
            .state,
        McpProjectionState::Removed,
        "S3"
    );
    assert!(
        witness_closed(&mut fresh.witness, Duration::from_secs(10)).await,
        "S3 the reaper, not a Host drop, stopped the child"
    );
    assert!(
        kept.status().expect("child status").is_some(),
        "S3 exited while a strong owner remained"
    );
    drop(host);
}

#[test]
#[ignore = "private child participant invoked by the native MCP supervision test"]
fn native_mcp_child_participant() {
    use std::io::{Read as _, Write as _};
    let Ok(address) = std::env::var(WITNESS_ENV) else {
        return;
    };
    let mut witness = std::net::TcpStream::connect(&address).unwrap();
    // Failure-only backstop: a parent that died without stopping this child
    // closes the witness, and this bound outlives every parent oracle.
    witness
        .set_read_timeout(Some(Duration::from_secs(120)))
        .unwrap();
    writeln!(witness, "ready:{}", std::process::id()).unwrap();
    let mut byte = [0u8; 1];
    let _ = witness.read(&mut byte);
}
