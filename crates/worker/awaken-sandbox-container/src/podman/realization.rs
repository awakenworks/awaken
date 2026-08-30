//! Podman physical-container naming, launch labels, and restore identity inspection.

use super::*;

impl PodmanRuntime {
    /// `podman container exists` reserves exit 1 for an absent target and 125
    /// for CLI/backend failure. Keeping the numeric status through the executor
    /// seam prevents a daemon outage from being misread as permission to create.
    pub(super) async fn container_exists(&self, container_id: &str) -> Result<bool, RuntimeError> {
        let args = [
            "container".to_string(),
            "exists".to_string(),
            container_id.to_string(),
        ];
        let out = self.exec.exec(&self.bin, &args).await.map_err(backend)?;
        match (out.ok, out.status_code) {
            (true, _) => Ok(true),
            (false, Some(1)) => Ok(false),
            _ => Err(RuntimeError::Backend(format!(
                "podman container exists: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    pub(super) fn container_run_args(
        &self,
        name: &str,
        plan: &ContainerPlan,
        extra_labels: impl IntoIterator<Item = (String, String)>,
    ) -> Vec<String> {
        let mut args = podman_run_argv(name, plan, &plan.rootfs);
        // Publish the agent's internal port to an ephemeral 127.0.0.1 host port so
        // `open_channel` can dial it (inserted after `--name <name>`, before the image).
        if let Some(i) = args.iter().position(|argument| argument == name) {
            let mut additions = vec![
                "--label".to_string(),
                format!("{RUNTIME_OWNER_LABEL}={}", self.owner_id),
            ];
            for (key, value) in extra_labels {
                additions.extend(["--label".to_string(), format!("{key}={value}")]);
            }
            additions.extend(["-p".to_string(), format!("127.0.0.1::{}", self.agent_port)]);
            args.splice(i + 1..i + 1, additions);
        }
        args
    }

    pub(super) async fn exact_restoration_id_with_state(
        &self,
        container_id: &str,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<(String, bool), RuntimeError> {
        let (observed_id, observed, observed_plan, running) = self
            .inspect_restoration_identity_with_state(container_id)
            .await?;
        if observed.as_ref() != Some(evidence) {
            return Err(backend(
                "Podman restore target belongs to a different exact effect",
            ));
        }
        if observed_plan.as_deref() != Some(plan_fingerprint) {
            return Err(backend(
                "Podman restore target belongs to a different immutable plan",
            ));
        }
        Ok((observed_id, running))
    }

    pub(super) async fn inspect_restoration_identity(
        &self,
        container_id: &str,
    ) -> Result<
        (
            String,
            Option<pc::SandboxRestorationEvidence>,
            Option<String>,
        ),
        RuntimeError,
    > {
        let (id, evidence, plan, running) = self
            .inspect_restoration_identity_with_state(container_id)
            .await?;
        if !running {
            return Err(backend("Podman exact restore target is not running"));
        }
        Ok((id, evidence, plan))
    }

    pub(super) async fn inspect_restoration_identity_with_state(
        &self,
        container_id: &str,
    ) -> Result<
        (
            String,
            Option<pc::SandboxRestorationEvidence>,
            Option<String>,
            bool,
        ),
        RuntimeError,
    > {
        let output = self
            .run(&[
                "inspect".into(),
                "--format".into(),
                "{{json .}}".into(),
                container_id.into(),
            ])
            .await?;
        let observed: serde_json::Value = serde_json::from_str(&output).map_err(backend)?;
        let labels = observed
            .pointer("/Config/Labels")
            .and_then(serde_json::Value::as_object);
        let restoration = crate::restoration_evidence_from_metadata(
            |key| {
                labels
                    .and_then(|labels| labels.get(key))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            },
            "Podman container",
        )?;
        let plan_fingerprint = labels
            .and_then(|labels| labels.get(crate::RESTORE_PLAN_LABEL))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let running = observed
            .pointer("/State/Running")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let observed_id = observed
            .get("Id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| container_id.to_string());
        Ok((observed_id, restoration, plan_fingerprint, running))
    }

    pub(super) async fn inspect_restoration(
        &self,
        container_id: &str,
    ) -> Result<(String, Option<pc::SandboxRestorationEvidence>), RuntimeError> {
        self.inspect_restoration_identity(container_id)
            .await
            .map(|(id, evidence, _)| (id, evidence))
    }

    pub(super) async fn live_inputs_root(
        &self,
        container_id: &str,
    ) -> Result<PathBuf, RuntimeError> {
        let output = self
            .run(&[
                "inspect".into(),
                "--format".into(),
                "{{json .Mounts}}".into(),
                container_id.into(),
            ])
            .await?;
        serde_json::from_str::<Vec<PodmanMount>>(&output)
            .map_err(backend)?
            .into_iter()
            .find(|mount| mount.destination == crate::LIVE_INPUTS_ROOT)
            .map(|mount| mount.source)
            .ok_or_else(|| backend("Podman container has no managed live-input root bind"))
    }
}
