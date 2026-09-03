//! Kubernetes projection of authority-resolved Memory snapshots.
//!
//! The Worker-side canonical `MemoryMounter` resolves claim-fenced bytes. This
//! adapter only streams the resulting bounded archive into a pod-local volume;
//! it owns no Resource client, database, or durable Memory state.

use super::*;

pub(super) const PROJECTOR: &str = "memory-projector";
pub(super) const PROJECTION_VOLUME: &str = "projection-ready";
pub(super) const PROJECTION_ROOT: &str = "/run/awaken-projection";
pub(super) const PROJECTION_FENCE_ENV: &str = "AWAKEN_PROJECTION_FENCE";
const PROJECTION_MARKER: &str = "/run/awaken-projection/complete";

pub(super) fn projection_fence_value(effect_fence: &ContainerEffectFence) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"awaken-k8s-projection-fence/v1\0");
    for value in [
        effect_fence.operation_id.as_str(),
        effect_fence.owner.as_str(),
        effect_fence.runtime_incarnation.as_str(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(&effect_fence.epoch.to_be_bytes());
    hasher.finalize().to_hex().to_string()
}

pub(super) fn gated_agent_command(
    command: &[String],
    effect_fence: &ContainerEffectFence,
) -> Vec<String> {
    let mut gated = vec![
        "/bin/sh".into(),
        "-ec".into(),
        format!(
            "expected=$1; marker={PROJECTION_MARKER}; while [ \"$(cat -- \"$marker\" 2>/dev/null || :)\" != \"$expected\" ]; do sleep 0.1; done; shift; exec \"$@\""
        ),
        "awaken-projection-gate".into(),
        projection_fence_value(effect_fence),
    ];
    gated.extend(command.iter().cloned());
    gated
}

async fn marker_command(
    runtime: &K8sRuntime,
    container_id: &str,
    effect_fence: &ContainerEffectFence,
    script: &str,
) -> Result<Vec<u8>, RuntimeError> {
    crate::runtime::validate_runtime_effect_fence(effect_fence)?;
    let expected = projection_fence_value(effect_fence);
    let argv = vec![
        "/bin/sh".into(),
        "-ec".into(),
        script.into(),
        "awaken-projection-marker".into(),
        expected,
        PROJECTION_MARKER.into(),
    ];
    let mut attached = runtime
        .pods()
        .exec(
            container_id,
            argv,
            &AttachParams::default()
                .container(PROJECTOR)
                .stdout(true)
                .stderr(false),
        )
        .await
        .map_err(backend)?;
    let mut stdout = attached
        .stdout()
        .ok_or_else(|| backend("k8s projection marker command has no stdout"))?;
    let status = attached
        .take_status()
        .ok_or_else(|| backend("k8s projection marker command has no completion status"))?;
    let mut bytes = Vec::new();
    let (read, status) = tokio::join!(stdout.read_to_end(&mut bytes), status);
    read.map_err(backend)?;
    if status.as_ref().and_then(|status| status.status.as_deref()) != Some("Success") {
        return Err(backend(
            "k8s projection marker command failed its effect fence",
        ));
    }
    Ok(bytes)
}

pub(super) async fn projection_complete(
    runtime: &K8sRuntime,
    container_id: &str,
    effect_fence: &ContainerEffectFence,
) -> Result<bool, RuntimeError> {
    let expected = projection_fence_value(effect_fence);
    let observed = marker_command(
        runtime,
        container_id,
        effect_fence,
        "expected=$1; marker=$2; [ \"$AWAKEN_PROJECTION_FENCE\" = \"$expected\" ]; if [ -e \"$marker\" ]; then [ -f \"$marker\" ] || exit 71; cat -- \"$marker\"; fi",
    )
    .await?;
    if observed.is_empty() {
        return Ok(false);
    }
    if observed == expected.as_bytes() {
        return Ok(true);
    }
    Err(backend(
        "k8s projection completion marker belongs to a different effect",
    ))
}

pub(super) async fn mark_projection_complete(
    runtime: &K8sRuntime,
    container_id: &str,
    effect_fence: &ContainerEffectFence,
) -> Result<(), RuntimeError> {
    marker_command(
        runtime,
        container_id,
        effect_fence,
        "expected=$1; marker=$2; [ \"$AWAKEN_PROJECTION_FENCE\" = \"$expected\" ]; tmp=\"$marker.tmp.$$\"; umask 077; printf %s \"$expected\" > \"$tmp\"; mv -f -- \"$tmp\" \"$marker\"",
    )
    .await
    .map(|_| ())
}

pub(super) async fn project_snapshots(
    runtime: &K8sRuntime,
    container_id: &str,
    plan: &ContainerPlan,
    effect_fence: &ContainerEffectFence,
) -> Result<(), RuntimeError> {
    crate::runtime::validate_runtime_effect_fence(effect_fence)?;
    let expected = projection_fence_value(effect_fence);
    for (index, mount) in plan.memory_mounts.iter().enumerate() {
        let root = format!("/memory/{index}");
        let argv = vec![
            "/bin/sh".into(),
            "-ec".into(),
            "expected=$1; root=$2; [ \"$AWAKEN_PROJECTION_FENCE\" = \"$expected\" ]; rm -rf -- \"$root\"/* \"$root\"/.[!.]* \"$root\"/..?*; tar -xf - -C \"$root\"".into(),
            "awaken-memory-project".into(),
            expected.clone(),
            root,
        ];
        let mut attached = runtime
            .pods()
            .exec(
                container_id,
                argv,
                &AttachParams::default()
                    .container(PROJECTOR)
                    .stdin(true)
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(backend)?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| backend("k8s Memory projector has no stdout"))?;
        let status = attached
            .take_status()
            .ok_or_else(|| backend("k8s Memory projector has no completion status"))?;
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| backend("k8s Memory projector has no stdin"))?;
        stdin
            .write_all(&mount.snapshot_tar)
            .await
            .map_err(backend)?;
        stdin.shutdown().await.map_err(backend)?;
        let mut ignored = Vec::new();
        stdout.read_to_end(&mut ignored).await.map_err(backend)?;
        let status = status.await;
        if status.as_ref().and_then(|status| status.status.as_deref()) != Some("Success") {
            return Err(backend(format!(
                "k8s Memory projector failed for mount {}",
                mount.mount_path
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(
        operation: &str,
        owner: &str,
        runtime: &str,
        epoch: u64,
        expiry: u64,
    ) -> ContainerEffectFence {
        ContainerEffectFence::new(operation, owner, runtime, epoch, expiry).unwrap()
    }

    #[test]
    fn projection_fence_and_agent_gate_decision_table_is_total() {
        /* Projection-gate cause/effect table. Causes: C1 exact operation/owner/
         * runtime/epoch; C2 lease expiry is renewed without changing that physical
         * effect; C3 any immutable effect coordinate differs. Effects: E1 C1 binds
         * the projector environment, marker, and Agent gate to one value; E2 C2
         * preserves that value so response-loss replay cannot clear Memory twice;
         * E3 C3 produces a distinct value and the projector fails before mutation.
         */
        let original = fence("operation-1", "owner-1", "runtime-1", 7, 100);
        let renewed = fence("operation-1", "owner-1", "runtime-1", 7, 200);
        let expected = projection_fence_value(&original);
        assert_eq!(projection_fence_value(&renewed), expected, "E2");

        for foreign in [
            fence("operation-2", "owner-1", "runtime-1", 7, 100),
            fence("operation-1", "owner-2", "runtime-1", 7, 100),
            fence("operation-1", "owner-1", "runtime-2", 7, 100),
            fence("operation-1", "owner-1", "runtime-1", 8, 100),
        ] {
            assert_ne!(projection_fence_value(&foreign), expected, "E3");
        }

        let command = vec!["agent".into(), "--serve".into()];
        let gated = gated_agent_command(&command, &original);
        assert_eq!(gated[0..2], ["/bin/sh", "-ec"], "E1");
        assert_eq!(gated[4], expected, "E1");
        assert_eq!(gated[5..], command, "E1");
    }
}
