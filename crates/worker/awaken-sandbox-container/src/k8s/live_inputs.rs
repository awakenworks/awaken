//! Kubernetes realization of the live, read-only Managed File projection.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) const VOLUME: &str = "live-inputs";
pub(super) const PROJECTOR: &str = "input-projector";

pub(super) fn append_projection(
    plan: &ContainerPlan,
    volumes: &mut Vec<Volume>,
    agent_mounts: &mut Vec<VolumeMount>,
    sidecars: &mut Vec<Container>,
) {
    // One Pod-owned input tree is mounted read-only into the untrusted Agent
    // and read-write only into the runtime projector. Both initial Files and
    // later generations are projected through that one runtime-owned channel,
    // preserving OS-enforced read-only semantics without making mutable input
    // membership part of the Pod realization identity.
    volumes.push(Volume {
        name: VOLUME.into(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    });
    agent_mounts.push(VolumeMount {
        name: VOLUME.into(),
        mount_path: crate::LIVE_INPUTS_ROOT.into(),
        read_only: Some(true),
        ..Default::default()
    });
    sidecars.push(Container {
        name: PROJECTOR.into(),
        image: Some(plan.image.clone()),
        command: Some(crate::environment_keepalive_command()),
        volume_mounts: Some(vec![VolumeMount {
            name: VOLUME.into(),
            mount_path: crate::LIVE_INPUTS_ROOT.into(),
            read_only: Some(false),
            ..Default::default()
        }]),
        security_context: Some(hardened_security_context()),
        ..Default::default()
    });
}

fn bytes(bind: &BindPlan) -> Option<&[u8]> {
    bind.content
        .as_deref()
        .map(str::as_bytes)
        .or(bind.content_bytes.as_deref())
}

async fn clear(runtime: &K8sRuntime, container_id: &str) -> Result<(), RuntimeError> {
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "root=$1; rm -rf -- \"$root\"/* \"$root\"/.[!.]* \"$root\"/..?*".into(),
        "awaken-input-clear".into(),
        crate::LIVE_INPUTS_ROOT.into(),
    ];
    exec(runtime, container_id, argv, None).await
}

pub(super) async fn project_manifest(
    runtime: &K8sRuntime,
    container_id: &str,
    plan: &ContainerPlan,
) -> Result<(), RuntimeError> {
    // Environment creation/recovery is a consistency boundary: no attempt is
    // released until this exact desired tree succeeds. Clearing first prevents a
    // removed File from surviving a host restart; any partial failure is retryable
    // against the same stable Pod and begins by clearing again.
    clear(runtime, container_id).await?;
    for bind in content_binds(plan)
        .into_iter()
        .filter(|bind| crate::live_inputs::manages(bind))
    {
        let contents = bytes(bind)
            .ok_or_else(|| backend("managed live input did not carry resolved bytes"))?;
        project(runtime, container_id, &bind.mount_path, contents).await?;
    }
    Ok(())
}

async fn exec(
    runtime: &K8sRuntime,
    container_id: &str,
    argv: Vec<String>,
    bytes: Option<&[u8]>,
) -> Result<(), RuntimeError> {
    let mut attached = runtime
        .pods()
        .exec(
            container_id,
            argv,
            &AttachParams::default()
                .container(PROJECTOR)
                .stdin(bytes.is_some())
                .stdout(true)
                .stderr(false),
        )
        .await
        .map_err(backend)?;
    let mut stdout = attached
        .stdout()
        .ok_or_else(|| backend("k8s input projector has no stdout"))?;
    let status = attached
        .take_status()
        .ok_or_else(|| backend("k8s input projector has no completion status"))?;
    if let Some(bytes) = bytes {
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| backend("k8s input projector has no stdin"))?;
        stdin.write_all(bytes).await.map_err(backend)?;
        stdin.shutdown().await.map_err(backend)?;
    }
    let mut ignored = Vec::new();
    stdout.read_to_end(&mut ignored).await.map_err(backend)?;
    let status = status.await;
    if status.as_ref().and_then(|status| status.status.as_deref()) != Some("Success") {
        return Err(backend("k8s input projector command failed"));
    }
    Ok(())
}

pub(super) async fn project(
    runtime: &K8sRuntime,
    container_id: &str,
    path: &str,
    bytes: &[u8],
) -> Result<(), RuntimeError> {
    crate::live_input_relative_path(path)
        .ok_or_else(|| backend("live input path escaped its projection root"))?;
    let temp = format!(
        "{path}.awaken-{}.tmp",
        EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "target=$1; temp=$2; mkdir -p \"$(dirname -- \"$target\")\"; cat > \"$temp\"; chmod 0444 \"$temp\"; mv -f -- \"$temp\" \"$target\"".into(),
        "awaken-input-project".into(),
        path.into(),
        temp,
    ];
    exec(runtime, container_id, argv, Some(bytes)).await
}

pub(super) async fn remove(
    runtime: &K8sRuntime,
    container_id: &str,
    path: &str,
) -> Result<(), RuntimeError> {
    crate::live_input_relative_path(path)
        .ok_or_else(|| backend("live input path escaped its projection root"))?;
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "target=$1; root=$2; rm -f -- \"$target\"; dir=$(dirname -- \"$target\"); while [ \"$dir\" != \"$root\" ] && [ \"${dir#\"$root\"/}\" != \"$dir\" ]; do rmdir -- \"$dir\" 2>/dev/null || break; dir=$(dirname -- \"$dir\"); done".into(),
        "awaken-input-remove".into(),
        path.into(),
        crate::LIVE_INPUTS_ROOT.into(),
    ];
    exec(runtime, container_id, argv, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> ContainerPlan {
        ContainerPlan {
            image: "agent:1".into(),
            command: vec!["claude".into(), "--acp".into()],
            env: Vec::new(),
            packages: Default::default(),
            binds: vec![crate::BindPlan {
                source_ref: String::new(),
                mount_path: "/mnt/session/uploads/awaken-design/current/index.html".into(),
                read_only: true,
                content: Some("<h1>current</h1>".into()),
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            }],
            outputs_volume: "/mnt/session/outputs".into(),
            network: crate::NetworkMode::Open,
            limits: Default::default(),
            memory_mounts: Vec::new(),
            rootfs: crate::RootfsPlan::HostUserland,
        }
    }

    #[test]
    fn managed_files_do_not_change_the_stable_pod_realization() {
        /* Cause/effect Pod projection decision table — KP1/KP2:
         * C1 a resolved, read-only input is below /mnt/session/uploads; C2 its
         * membership/path/bytes are absent, A, or B; C3 the stable projector is
         * present. C1+C3 => E1 Agent mounts the one tree read-only and E2 only the
         * projector mounts it read-write. C1+C2+C3 => E3 every mutable manifest
         * produces the exact same Pod spec, E4 no ConfigMap/init path duplicates
         * projection. !C1 remains on the parent planner's ConfigMap path (KP3).
         */
        let with_a = plan();
        let mut with_b = plan();
        with_b.binds[0].mount_path = "/mnt/session/uploads/awaken-design/current/other.html".into();
        with_b.binds[0].content = Some("<h1>other generation</h1>".into());
        let mut without = plan();
        without.binds.clear();

        let spec = build_pod("live", &with_a, &None, "m", None, false, &[])
            .spec
            .unwrap();
        assert_eq!(
            spec,
            build_pod("live", &with_b, &None, "m", None, false, &[])
                .spec
                .unwrap()
        );
        assert_eq!(
            spec,
            build_pod("live", &without, &None, "m", None, false, &[])
                .spec
                .unwrap()
        );
        let agent = spec
            .containers
            .iter()
            .find(|item| item.name == "agent")
            .unwrap();
        let agent_live = agent
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|mount| mount.name == VOLUME)
            .unwrap();
        assert_eq!(agent_live.mount_path, crate::LIVE_INPUTS_ROOT);
        assert_eq!(agent_live.read_only, Some(true));
        assert!(
            agent
                .volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .all(|mount| mount.name != "cfg-0")
        );

        let projector = spec
            .containers
            .iter()
            .find(|item| item.name == PROJECTOR)
            .unwrap();
        assert_eq!(
            projector.volume_mounts.as_ref().unwrap()[0].read_only,
            Some(false)
        );
        assert!(
            spec.volumes
                .as_ref()
                .unwrap()
                .iter()
                .all(|volume| volume.config_map.is_none())
        );
        assert!(spec.init_containers.is_none());
    }
}
