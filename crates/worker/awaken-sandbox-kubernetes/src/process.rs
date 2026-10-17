// @formal evidence session_execution.one_physical_attempt_per_thread
//! Kubernetes attached-process lifecycle.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::Api;
use kube::api::AttachParams;
use tokio::io::AsyncReadExt;

use super::{EXEC_SEQUENCE, K8sRuntime, backend, channel};
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_connection_plan::{ChannelFactory as _, ConnectionPlan, TokioChannelFactory};
use awaken_sandbox_container::RuntimeAgentProcess;
use awaken_sandbox_container::RuntimeError;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt as _;

pub(super) struct K8sExecState {
    pub(super) completion: Option<tokio::task::JoinHandle<Option<Status>>>,
    // One disposable observation cache covers both exit and failed observation.
    // An error is never physical exit evidence or a durable stop receipt.
    pub(super) outcome: Option<Result<pc::ExitStatus, String>>,
}

pub(super) struct K8sExecProcess {
    pub(super) id: String,
    pub(super) pod: String,
    pub(super) pid_file: String,
    pub(super) pods: Api<Pod>,
    pub(super) state: tokio::sync::Mutex<K8sExecState>,
}

pub(super) struct K8sExecPlan {
    pub(super) pid_file: String,
    pub(super) argv: Vec<String>,
    pub(super) secret_stdin: Vec<pc::MaterializedEnvValue>,
}

/// Keep the SDK attachment (including its unrequested stdin writer) until the
/// status observer returns. On WebSocket v4, dropping that writer closes the
/// entire connection and can discard stdout/status. This owns lifetime only;
/// an absent remote status remains absent and never becomes successful exit.
pub(super) async fn observe_attached_completion<O, F>(owner: O, completion: F) -> Option<Status>
where
    F: std::future::Future<Output = Option<Status>>,
{
    let status = completion.await;
    drop(owner);
    status
}

pub(super) fn k8s_exit_status(status: Option<Status>) -> pc::ExitStatus {
    let success = status.as_ref().and_then(|status| status.status.as_deref()) == Some("Success");
    let code = status
        .and_then(|status| status.details)
        .and_then(|details| details.causes)
        .and_then(|causes| {
            causes.into_iter().find_map(|cause| {
                (cause.reason.as_deref() == Some("ExitCode"))
                    .then_some(cause.message)
                    .flatten()
            })
        })
        .and_then(|message| message.parse::<i32>().ok())
        .or(success.then_some(0));
    pc::ExitStatus {
        code,
        signaled: false,
    }
}

pub(super) fn k8s_live_file_result(
    status: Option<Status>,
    bytes: Vec<u8>,
) -> Result<Option<Vec<u8>>, RuntimeError> {
    let status = k8s_exit_status(status);
    if status.code == Some(0) {
        Ok(Some(bytes))
    } else {
        Err(backend(format!(
            "Kubernetes live-file read failed with exit code {:?}",
            status.code
        )))
    }
}

pub(super) fn signal_effect_is_complete(
    signal_status: &pc::ExitStatus,
    process_status: Option<&pc::ExitStatus>,
) -> bool {
    signal_status.code == Some(0) || process_status.is_some()
}

pub(super) fn k8s_exec_argv(
    id: &str,
    command: pc::MaterializedCommand,
) -> Result<K8sExecPlan, RuntimeError> {
    if command.argv.is_empty() {
        return Err(backend("exec command argv is empty"));
    }
    let pid_file = awaken_sandbox_container::engine::exec_pid_file(id);
    // Null commands observe EOF while the v4 transport writer stays alive.
    let stdin_redirect = if command.stdio == pc::Stdio::Null {
        " </dev/null"
    } else {
        ""
    };
    let mut inline_env = Vec::new();
    let mut secrets = Vec::new();
    for var in command.env {
        match var.value {
            pc::MaterializedEnvValue::Inline(value) => {
                inline_env.push(format!("{}={value}", var.name));
            }
            pc::MaterializedEnvValue::Secret(value) => {
                secrets.push((var.name, pc::MaterializedEnvValue::Secret(value)));
            }
        }
    }
    let mut argv = vec![
        "sh".into(),
        "-c".into(),
        format!(
            "pid_file=$1; cwd=$2; secret_count=$3; shift 3; \
             while [ \"$secret_count\" -gt 0 ]; do \
               secret_name=$1; secret_length=$2; shift 2; \
               secret_value=$(dd bs=1 count=\"$secret_length\" 2>/dev/null; printf .) || exit 125; \
               secret_value=${{secret_value%.}}; export \"$secret_name=$secret_value\" || exit 125; \
               secret_count=$((secret_count - 1)); \
             done; \
             {} \
             record_exec_identity; \
             if [ -n \"$cwd\" ]; then cd -- \"$cwd\" || exit 126; fi; exec \"$@\"{}",
            awaken_sandbox_container::engine::exec_pid_file_prelude(),
            stdin_redirect
        ),
        "awaken-exec".into(),
        pid_file.clone(),
        command.cwd,
        secrets.len().to_string(),
    ];
    for (name, value) in &secrets {
        argv.push(name.clone());
        argv.push(value.expose().len().to_string());
    }
    argv.push("env".into());
    argv.extend(inline_env);
    argv.extend(command.argv);
    Ok(K8sExecPlan {
        pid_file,
        argv,
        secret_stdin: secrets.into_iter().map(|(_, value)| value).collect(),
    })
}

#[async_trait]
impl pc::ProcessHandle for K8sExecProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        awaken_local_process::wait_for_process_exit(self).await
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let mut state = self.state.lock().await;
        if state.outcome.is_none() {
            let Some(completion) = state.completion.as_ref() else {
                return Err(pc::SandboxError::transport(
                    "k8s exec completion is unavailable",
                ));
            };
            if !completion.is_finished() {
                return Ok(None);
            }
            let completion = state.completion.take().expect("checked above");
            state.outcome = Some(match completion.await {
                Ok(observation) => {
                    let status = k8s_exit_status(observation.clone());
                    if status.code.is_some() {
                        Ok(status)
                    } else {
                        // Numeric API code and frame presence diagnose observation
                        // failure without copying arbitrary provider text/commands.
                        Err(format!(
                            "Kubernetes exec ended without an observed exit status (status_frame={}, api_code={:?})",
                            observation.is_some(),
                            observation.as_ref().and_then(|value| value.code),
                        ))
                    }
                }
                Err(error) => Err(if error.is_cancelled() {
                    "Kubernetes exec completion observer was cancelled"
                } else {
                    "Kubernetes exec completion observer failed"
                }
                .into()),
            });
        }
        state
            .outcome
            .as_ref()
            .expect("finished observer is cached")
            .clone()
            .map(Some)
            .map_err(|error| pc::SandboxError::backend("poll", error))
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        if self.poll().await?.is_some() {
            return Ok(());
        }
        let name = match signal {
            pc::Signal::Term => "TERM",
            pc::Signal::Kill => "KILL",
            pc::Signal::Int => "INT",
        };
        let script = format!(
            "pid=$(cat -- '{}') && kill -{} \"$pid\"",
            self.pid_file.replace('\'', "'\\''"),
            name
        );
        let mut attached = self
            .pods
            .exec(
                &self.pod,
                vec!["sh", "-c", &script],
                &AttachParams::default()
                    .container("agent")
                    .stdin(false)
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(|error| pc::SandboxError::backend("signal", error))?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| pc::SandboxError::backend("signal", "k8s signal exec has no stdout"))?;
        let status = attached
            .take_status()
            .ok_or_else(|| pc::SandboxError::backend("signal", "k8s signal exec has no status"))?;
        let mut ignored = Vec::new();
        let (read, status) = tokio::join!(stdout.read_to_end(&mut ignored), status);
        read.map_err(|error| pc::SandboxError::backend("signal", error))?;
        let signal_status = k8s_exit_status(status);
        let process_status = if signal_status.code == Some(0) {
            None
        } else {
            self.poll().await?
        };
        if signal_effect_is_complete(&signal_status, process_status.as_ref()) {
            Ok(())
        } else {
            Err(pc::SandboxError::backend(
                "signal",
                "k8s exec signal failed",
            ))
        }
    }
}

pub(super) async fn spawn_exec(
    runtime: &K8sRuntime,
    admitted: awaken_sandbox_container::AdmittedExec,
    command: pc::MaterializedCommand,
) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
    let container_id = admitted.container_id();
    if command.stdio == pc::Stdio::Piped {
        return Err(backend(
            "piped container exec requires the agent-channel capability",
        ));
    }
    let id = format!(
        "k8s-exec-{}-{}",
        std::process::id(),
        EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let execution = k8s_exec_argv(&id, command)?;
    let pods = runtime.streaming_pods();
    let mut attached = pods
        .exec(
            container_id,
            execution.argv,
            &AttachParams::default()
                .container("agent")
                .stdin(!execution.secret_stdin.is_empty())
                // kube requires at least one attached stdio stream. Keep stdout
                // attached and drain it in the completion task so a noisy command
                // cannot block before the remote status frame is delivered.
                .stdout(true)
                .stderr(false),
        )
        .await
        .map_err(backend)?;
    let mut secret_input = None;
    if !execution.secret_stdin.is_empty() {
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| backend("k8s secret prelude has no stdin"))?;
        for secret in execution.secret_stdin {
            stdin
                .write_all(secret.expose().as_bytes())
                .await
                .map_err(backend)?;
        }
        // The prelude consumes exact byte lengths, not EOF. Keep this
        // writer until status so a v4 connection cannot close prematurely.
        stdin.flush().await.map_err(backend)?;
        secret_input = Some(stdin);
    }
    let mut stdout = attached
        .stdout()
        .ok_or_else(|| backend("k8s exec has no stdout"))?;
    let status = attached
        .take_status()
        .ok_or_else(|| backend("k8s exec has no completion status"))?;
    let completion = tokio::spawn(super::process::observe_attached_completion(
        attached,
        async move {
            let mut ignored = Vec::new();
            let (_, status) = tokio::join!(stdout.read_to_end(&mut ignored), status);
            drop(secret_input);
            status
        },
    ));
    Ok(Box::new(K8sExecProcess {
        id,
        pod: container_id.to_string(),
        pid_file: execution.pid_file,
        pods,
        state: tokio::sync::Mutex::new(K8sExecState {
            completion: Some(completion),
            outcome: None,
        }),
    }))
}

pub(super) async fn spawn_agent(
    runtime: &K8sRuntime,
    admitted: awaken_sandbox_container::AdmittedExec,
    command: pc::MaterializedCommand,
) -> Result<RuntimeAgentProcess, RuntimeError> {
    let container_id = admitted.container_id();
    let id = format!(
        "k8s-agent-exec-{}-{}",
        std::process::id(),
        EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let execution = k8s_exec_argv(&id, command)?;
    let pods = runtime.streaming_pods();
    let mut attached = pods
        .exec(
            container_id,
            execution.argv,
            &AttachParams::default()
                .container("agent")
                .stdin(true)
                .stdout(true)
                .stderr(false),
        )
        .await
        .map_err(backend)?;
    let stdin = attached
        .stdin()
        .ok_or_else(|| backend("k8s agent exec has no stdin"))?;
    let mut stdin = stdin;
    for secret in execution.secret_stdin {
        stdin
            .write_all(secret.expose().as_bytes())
            .await
            .map_err(backend)?;
    }
    stdin.flush().await.map_err(backend)?;
    let stdout = attached
        .stdout()
        .ok_or_else(|| backend("k8s agent exec has no stdout"))?;
    let completion = attached
        .take_status()
        .ok_or_else(|| backend("k8s agent exec has no completion status"))?;
    Ok(RuntimeAgentProcess {
        process: Box::new(K8sExecProcess {
            id,
            pod: container_id.to_string(),
            pid_file: execution.pid_file,
            pods,
            state: tokio::sync::Mutex::new(K8sExecState {
                completion: Some(tokio::spawn(super::process::observe_attached_completion(
                    attached, completion,
                ))),
                outcome: None,
            }),
        }),
        channel: Box::new(SplitChannel::new(stdout, stdin)),
    })
}

pub(super) async fn open_agent_channel(
    runtime: &K8sRuntime,
    container_id: &str,
) -> Result<Box<dyn AgentChannel>, RuntimeError> {
    if let Some(port) = runtime.pod_channel_port {
        return channel::open_pod_channel(&runtime.streaming_pods(), container_id, port).await;
    }
    match runtime.rendezvous {
        // Reverse-dial: the host listens, the egress-fenced Pod dials out to us.
        Some(addr) => channel::accept_reverse(addr).await,
        // Direct-dial the agent's stdio over its published Service.
        None => TokioChannelFactory
            .connect(&ConnectionPlan::tcp_dial(runtime.agent_addr.to_string()))
            .await
            .map_err(backend),
    }
}

pub(super) async fn read_live_file(
    runtime: &K8sRuntime,
    container_id: &str,
    path: &str,
) -> Result<Option<Vec<u8>>, RuntimeError> {
    let mut attached = runtime
        .streaming_pods()
        .exec(
            container_id,
            vec!["cat", "--", path],
            &AttachParams::default().container("agent").stderr(false),
        )
        .await
        .map_err(backend)?;
    let mut stdout = attached
        .stdout()
        .ok_or_else(|| backend("k8s credential harvest has no stdout"))?;
    let status = attached
        .take_status()
        .ok_or_else(|| backend("k8s credential harvest has no completion status"))?;
    let mut bytes = Vec::new();
    let (read, status) = tokio::join!(stdout.read_to_end(&mut bytes), status);
    read.map_err(backend)?;
    k8s_live_file_result(status, bytes)
}
