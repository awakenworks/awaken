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
    init_containers: &mut Vec<Container>,
) {
    // One Pod-owned input tree is mounted read-only into the untrusted Agent
    // and read-write only into the runtime projector. Initial Files seed this
    // same volume below; later File generations replace bytes through an exec
    // into the projector, preserving OS-enforced read-only semantics without
    // changing the Pod or Session identity.
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

    // Every item remains backed by its immutable ConfigMap. Managed inputs seed
    // the shared tree; other paths are mounted directly by the parent planner.
    let mut seed_mounts = vec![VolumeMount {
        name: VOLUME.into(),
        mount_path: "/live".into(),
        read_only: Some(false),
        ..Default::default()
    }];
    let mut seed_argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "shift; while [ \"$#\" -gt 0 ]; do source=$1; target=$2; mkdir -p \"$(dirname -- \"$target\")\"; cp -- \"$source\" \"$target\"; chmod 0444 \"$target\"; shift 2; done".into(),
        "awaken-input-seed".into(),
    ];
    for (i, bind) in content_binds(plan).iter().enumerate() {
        if !bind.read_only {
            continue;
        }
        let Some(relative) = crate::live_input_relative_path(&bind.mount_path) else {
            continue;
        };
        let seed_path = format!("/seed/{i}");
        seed_mounts.push(VolumeMount {
            name: format!("cfg-{i}"),
            mount_path: seed_path.clone(),
            read_only: Some(true),
            ..Default::default()
        });
        seed_argv.push(format!("{seed_path}/{CONFIGMAP_KEY}"));
        seed_argv.push(format!("/live/{relative}"));
    }
    if seed_argv.len() > 4 {
        init_containers.push(Container {
            name: "input-seed".into(),
            image: Some(plan.image.clone()),
            command: Some(seed_argv),
            volume_mounts: Some(seed_mounts),
            security_context: Some(hardened_security_context()),
            ..Default::default()
        });
    }
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
        "rm -f -- \"$1\"".into(),
        "awaken-input-remove".into(),
        path.into(),
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
    fn managed_files_seed_the_same_read_only_tree_used_for_live_replacement() {
        /* Cause/effect Pod projection table — KP1:
         * C1 content targets /mnt/session/uploads; C2 it is read-only.
         * C1+C2 => E1 ConfigMap seeds the shared emptyDir in init, E2 Agent mounts
         * that tree read-only, E3 only the projector mounts it read-write, and E4
         * no subPath shadows later replacement. A path outside C1 retains the
         * existing exact ConfigMap subPath behavior in the parent planner tests.
         */
        let spec = build_pod("live", &plan(), &None, "m", None, false, &[])
            .spec
            .unwrap();
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
        let seed = spec
            .init_containers
            .as_ref()
            .unwrap()
            .iter()
            .find(|item| item.name == "input-seed")
            .unwrap();
        let command = seed.command.as_ref().unwrap();
        assert!(
            command
                .iter()
                .any(|part| part == "/live/awaken-design/current/index.html")
        );
        assert!(
            seed.volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .any(|mount| mount.name == "cfg-0" && mount.mount_path == "/seed/0")
        );
    }
}
