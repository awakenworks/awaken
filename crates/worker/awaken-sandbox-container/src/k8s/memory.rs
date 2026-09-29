//! Kubernetes projection of authority-resolved Memory snapshots.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) const PROJECTOR: &str = "memory-projector";

pub(super) async fn project_snapshots(
    runtime: &K8sRuntime,
    container_id: &str,
    plan: &ContainerPlan,
    effect_fence: &ContainerEffectFence,
) -> Result<(), RuntimeError> {
    runtime.validate_effect_fence(effect_fence)?;
    let expected = super::projection::projection_fence_value(effect_fence);
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
