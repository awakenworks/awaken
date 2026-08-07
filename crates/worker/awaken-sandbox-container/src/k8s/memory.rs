//! Kubernetes projection of authority-resolved Memory snapshots.
//!
//! The Worker-side canonical `MemoryMounter` resolves claim-fenced bytes. This
//! adapter only streams the resulting bounded archive into a pod-local volume;
//! it owns no Resource client, database, or durable Memory state.

use super::*;

pub(super) const PROJECTOR: &str = "memory-projector";

pub(super) async fn project_snapshots(
    runtime: &K8sRuntime,
    container_id: &str,
    plan: &ContainerPlan,
) -> Result<(), RuntimeError> {
    for (index, mount) in plan.memory_mounts.iter().enumerate() {
        let root = format!("/memory/{index}");
        let argv = vec![
            "/bin/sh".into(),
            "-ec".into(),
            "root=$1; rm -rf -- \"$root\"/* \"$root\"/.[!.]* \"$root\"/..?*; tar -xf - -C \"$root\"".into(),
            "awaken-memory-project".into(),
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
