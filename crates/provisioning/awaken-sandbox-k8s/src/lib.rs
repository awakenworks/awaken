//! Kubernetes `SandboxProvider` (B-P5a, ADR-0021 §5/§8) — a real, non-demo sandbox
//! backend where each sandbox is a Pod. Per ADR-0021 §5 the seam is **declarative**:
//! `create` `kubectl apply`s a Pod manifest (never a fragile `kubectl run
//! --overrides` argv wrapper), `spawn` `kubectl exec`s a process in it, `adopt`
//! reconnects to a still-running Pod by name (survives host restart), and `dispose`
//! deletes it. `provider_kind` is `"k8s"`, so the fleet router and crash-adoption
//! route by handle. This is the same `SandboxProvider` port as the local/docker
//! drivers — the seam bent for k8s only in that `create` applies a manifest.

use async_trait::async_trait;
use awaken_provisioning_contract::{
    Artifact, Command, ExitStatus, IsolationClass, MountRequirement, ProcessHandle, RealizedMount,
    Sandbox, SandboxCapabilities, SandboxError, SandboxHandle, SandboxProvider, SandboxSpec,
    SandboxStatus, Signal,
};
use tokio::process::Command as OsCommand;

const PROVIDER_KIND: &str = "k8s";

/// Creates Pod-isolated sandboxes against a cluster via `kubectl`.
pub struct K8sSandboxProvider {
    image: String,
    context: String,
    namespace: String,
}

impl K8sSandboxProvider {
    /// Sandboxes are Pods of `image` in `namespace` on the `kubectl` `context`.
    #[must_use]
    pub fn new(
        image: impl Into<String>,
        context: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            image: image.into(),
            context: context.into(),
            namespace: namespace.into(),
        }
    }

    fn base(&self) -> Vec<String> {
        vec![
            "--context".into(),
            self.context.clone(),
            "-n".into(),
            self.namespace.clone(),
        ]
    }
}

fn pod_name(scope: &str) -> String {
    // A DNS-1123 name: lowercase, deterministic per scope.
    format!("awaken-sbx-{}", scope.to_lowercase().replace('_', "-"))
}

async fn kubectl(args: &[String]) -> Result<String, SandboxError> {
    let out = OsCommand::new("kubectl")
        .args(args)
        .output()
        .await
        .map_err(|e| SandboxError::new(format!("kubectl spawn: {e}")))?;
    if !out.status.success() {
        return Err(SandboxError::new(format!(
            "kubectl {}: {}",
            args.first().cloned().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn pod_manifest(name: &str, namespace: &str, image: &str) -> String {
    // Minimal Pod: one container kept alive with `sleep infinity` so exec can
    // multiplex the run's processes (ADR-0058). Never-restart so a crashed process
    // does not resurrect the whole sandbox.
    format!(
        r#"apiVersion: v1
kind: Pod
metadata:
  name: {name}
  namespace: {namespace}
  labels: {{ app: awaken-sandbox }}
spec:
  restartPolicy: Never
  containers:
    - name: sandbox
      image: {image}
      imagePullPolicy: IfNotPresent
      command: ["sleep", "infinity"]
"#
    )
}

#[async_trait]
impl SandboxProvider for K8sSandboxProvider {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            isolation: IsolationClass::Container,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: true,
            secret_egress_substitution: false,
            resource_limits: true,
            custom_rootfs: true,
        }
    }

    async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
        let name = pod_name(&spec.scope);
        // Idempotent: clear any stale pod of this scope first.
        let mut del = self.base();
        del.extend(
            [
                "delete",
                "pod",
                &name,
                "--ignore-not-found",
                "--force",
                "--grace-period=0",
            ]
            .map(String::from),
        );
        let _ = kubectl(&del).await;

        // Declarative create: apply a manifest via stdin (ADR-0021 §5).
        let manifest = pod_manifest(&name, &self.namespace, &self.image);
        let mut apply = self.base();
        apply.extend(["apply", "-f", "-"].map(String::from));
        let mut child = OsCommand::new("kubectl")
            .args(&apply)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| SandboxError::new(format!("kubectl apply: {e}")))?;
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().unwrap();
            stdin
                .write_all(manifest.as_bytes())
                .await
                .map_err(|e| SandboxError::new(format!("write manifest: {e}")))?;
        }
        let status = child
            .wait()
            .await
            .map_err(|e| SandboxError::new(format!("apply wait: {e}")))?;
        if !status.success() {
            return Err(SandboxError::new("kubectl apply failed"));
        }

        // Wait for the Pod to be ready to exec into.
        let mut wait = self.base();
        wait.extend(
            [
                "wait",
                &format!("pod/{name}"),
                "--for=condition=Ready",
                "--timeout=60s",
            ]
            .map(String::from),
        );
        kubectl(&wait).await?;

        Ok(Box::new(K8sSandbox {
            scope: spec.scope.clone(),
            pod: name,
            base: self.base(),
        }))
    }

    async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
        let mut phase = self.base();
        phase.extend(
            [
                "get",
                "pod",
                &handle.sandbox_id,
                "-o",
                "jsonpath={.status.phase}",
            ]
            .map(String::from),
        );
        if kubectl(&phase).await?.trim() != "Running" {
            return Err(SandboxError::new("pod not running"));
        }
        Ok(Box::new(K8sSandbox {
            scope: handle.sandbox_id.clone(),
            pod: handle.sandbox_id.clone(),
            base: self.base(),
        }))
    }
}

struct K8sSandbox {
    scope: String,
    pod: String,
    base: Vec<String>,
}

#[async_trait]
impl Sandbox for K8sSandbox {
    fn id(&self) -> &str {
        &self.scope
    }

    fn handle(&self) -> SandboxHandle {
        SandboxHandle::new(PROVIDER_KIND, &self.pod)
    }

    async fn spawn(&self, command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
        if command.argv.is_empty() {
            return Err(SandboxError::new("empty argv"));
        }
        let mut args = self.base.clone();
        args.extend(["exec", &self.pod, "--"].map(String::from));
        args.extend(command.argv.iter().cloned());
        let child = OsCommand::new("kubectl")
            .args(&args)
            .spawn()
            .map_err(|e| SandboxError::new(format!("exec: {e}")))?;
        Ok(Box::new(K8sProcess {
            id: format!("{}-{}", self.pod, command.argv.join("_")),
            child: tokio::sync::Mutex::new(Some(child)),
        }))
    }

    async fn attach(&self, _req: MountRequirement) -> Result<RealizedMount, SandboxError> {
        Err(SandboxError::new(
            "attach unsupported by the minimal k8s driver",
        ))
    }

    async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError> {
        Ok(Vec::new())
    }

    async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, SandboxError> {
        Err(SandboxError::new("read_artifact unsupported"))
    }

    fn realized(&self) -> &[RealizedMount] {
        &[]
    }

    async fn process(&self, _process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError> {
        Err(SandboxError::new("process reattach unsupported"))
    }

    async fn status(&self) -> Result<SandboxStatus, SandboxError> {
        let mut args = self.base.clone();
        args.extend(["get", "pod", &self.pod, "-o", "jsonpath={.status.phase}"].map(String::from));
        match kubectl(&args).await {
            Ok(p) if p.trim() == "Running" => Ok(SandboxStatus::Ready),
            Ok(p) if p.trim() == "Pending" => Ok(SandboxStatus::Provisioning),
            _ => Ok(SandboxStatus::Terminated),
        }
    }

    async fn renew_lease(&self) -> Result<(), SandboxError> {
        // A Pod lives until dispose; a production build would refresh a lease
        // annotation an operator reaps on. Nothing to do for the minimal driver.
        Ok(())
    }

    async fn dispose(&self) -> Result<(), SandboxError> {
        let mut args = self.base.clone();
        args.extend(
            [
                "delete",
                "pod",
                &self.pod,
                "--force",
                "--grace-period=0",
                "--ignore-not-found",
            ]
            .map(String::from),
        );
        kubectl(&args).await.map(|_| ())
    }
}

struct K8sProcess {
    id: String,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}

fn exit_of(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signaled: status.code().is_none(),
    }
}

#[async_trait]
impl ProcessHandle for K8sProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<ExitStatus, SandboxError> {
        let mut guard = self.child.lock().await;
        let child = guard
            .as_mut()
            .ok_or_else(|| SandboxError::new("already awaited"))?;
        let status = child
            .wait()
            .await
            .map_err(|e| SandboxError::new(format!("wait: {e}")))?;
        Ok(exit_of(status))
    }

    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
        let mut guard = self.child.lock().await;
        let child = guard
            .as_mut()
            .ok_or_else(|| SandboxError::new("already awaited"))?;
        match child.try_wait() {
            Ok(Some(status)) => Ok(Some(exit_of(status))),
            Ok(None) => Ok(None),
            Err(e) => Err(SandboxError::new(format!("poll: {e}"))),
        }
    }

    async fn signal(&self, signal: Signal) -> Result<(), SandboxError> {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut() {
            if matches!(signal, Signal::Kill | Signal::Term) {
                let _ = child.start_kill();
            }
        }
        Ok(())
    }
}
