//! Kubernetes attached-process lifecycle.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::Api;
use kube::api::AttachParams;
use tokio::io::AsyncReadExt;

use super::backend;
use crate::RuntimeError;

pub(super) struct K8sExecState {
    pub(super) completion: Option<tokio::task::JoinHandle<Option<Status>>>,
    pub(super) status: Option<pc::ExitStatus>,
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
        .or(Some(if success { 0 } else { 1 }));
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

pub(super) fn k8s_exec_argv(
    id: &str,
    command: pc::MaterializedCommand,
) -> Result<K8sExecPlan, RuntimeError> {
    if command.argv.is_empty() {
        return Err(backend("exec command argv is empty"));
    }
    let pid_file = format!("/tmp/{id}.pid");
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
        "pid_file=$1; cwd=$2; secret_count=$3; shift 3; \
         while [ \"$secret_count\" -gt 0 ]; do \
           secret_name=$1; secret_length=$2; shift 2; \
           secret_value=$(dd bs=1 count=\"$secret_length\" 2>/dev/null; printf .) || exit 125; \
           secret_value=${secret_value%.}; export \"$secret_name=$secret_value\" || exit 125; \
           secret_count=$((secret_count - 1)); \
         done; \
         printf '%s' \"$$\" > \"$pid_file\"; \
         if [ -n \"$cwd\" ]; then cd -- \"$cwd\" || exit 126; fi; exec \"$@\""
            .into(),
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
        let mut state = self.state.lock().await;
        if let Some(status) = &state.status {
            return Ok(status.clone());
        }
        let completion = state
            .completion
            .take()
            .ok_or_else(|| pc::SandboxError::new("k8s exec completion is unavailable"))?;
        let status = completion
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let status = k8s_exit_status(status);
        state.status = Some(status.clone());
        Ok(status)
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let mut state = self.state.lock().await;
        if let Some(status) = &state.status {
            return Ok(Some(status.clone()));
        }
        let Some(completion) = state.completion.as_ref() else {
            return Err(pc::SandboxError::new("k8s exec completion is unavailable"));
        };
        if !completion.is_finished() {
            return Ok(None);
        }
        let completion = state.completion.take().expect("checked above");
        let status = completion
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let status = k8s_exit_status(status);
        state.status = Some(status.clone());
        Ok(Some(status))
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
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
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| pc::SandboxError::new("k8s signal exec has no stdout"))?;
        let status = attached
            .take_status()
            .ok_or_else(|| pc::SandboxError::new("k8s signal exec has no status"))?;
        let mut ignored = Vec::new();
        let (read, status) = tokio::join!(stdout.read_to_end(&mut ignored), status);
        read.map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if k8s_exit_status(status).code == Some(0) {
            Ok(())
        } else {
            Err(pc::SandboxError::new("k8s exec signal failed"))
        }
    }
}
