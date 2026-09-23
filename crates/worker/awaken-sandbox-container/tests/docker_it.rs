//! Real Docker integration (ADR-0041 Slice 5), gated on the `docker` feature AND a
//! reachable daemon — skips cleanly otherwise (same discipline as the bwrap tests).
//!
//! Run with: `cargo test -p awaken-sandbox-container --features docker --test docker_it`
#![cfg(feature = "docker")]

#[path = "common/restore.rs"]
mod common;
#[path = "common/disposal.rs"]
mod disposal;
#[path = "common/host_staging.rs"]
mod host_staging;
#[path = "common/process.rs"]
mod process_lifecycle;
#[path = "common/residue.rs"]
mod residue;

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_sandbox_container::RestoreTargetContainerEnvironmentProvider as _;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{
    ContainerPlan, ContainerProvider, ContainerRuntime, ContainerState, NetworkMode, RootfsPlan,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const AGENT_PORT: u16 = 8080;

/// The in-container file the signalled exec writes before it settles into its
/// sleep, and so the observable proof that its wrapper already recorded the pid
/// every later signal resolves.
const EXEC_ESTABLISHED_MARKER: &str = "/tmp/awaken-it-exec.established";

/// The same evidence for the reaping test, in its own container.
const REAP_ESTABLISHED_MARKER: &str = "/tmp/awaken-it-reap.established";

fn plan(cmd: &[&str]) -> ContainerPlan {
    ContainerPlan {
        image: "busybox:latest".into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        egress_identity: Default::default(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        memory_mounts: Vec::new(),
        rootfs: RootfsPlan::HostUserland,
    }
}

async fn runtime() -> Option<DockerRuntime> {
    let rt = DockerRuntime::connect_local(AGENT_PORT).ok()?;
    if rt.ping().await.is_err() {
        eprintln!("skipping: no reachable Docker daemon");
        return None;
    }
    Some(rt)
}

#[tokio::test]
async fn docker_ordinary_host_staging_survives_creator_replacement() {
    let Some(first) = runtime().await else { return };
    let Some(peer) = runtime().await else {
        panic!("Docker disappeared after readiness")
    };
    host_staging::verify_ordinary_host_staging_recovery(
        Arc::new(first),
        Arc::new(peer),
        "busybox:latest",
        residue::Backend::Docker,
    )
    .await;
}

#[tokio::test]
async fn docker_full_lifecycle_against_a_real_daemon() {
    let Some(rt) = runtime().await else { return };
    let container = residue::create_owned(
        &rt,
        "it-life",
        &plan(&["sleep", "30"]),
        residue::Backend::Docker,
    )
    .await;
    let id = container.id();

    // process-as-container: the sleep is the container's main process, running.
    assert!(matches!(
        rt.inspect(id).await.unwrap(),
        ContainerState::Running
    ));
    assert!(rt.poll(id).await.unwrap().is_none(), "still running");

    // reap + teardown
    rt.signal(id, pc::Signal::Kill).await.unwrap();
    rt.remove(id).await.unwrap();
    // once removed, inspect no longer finds it running
    assert!(
        rt.inspect(id).await.is_err() || matches!(rt.inspect(id).await, Ok(ContainerState::Gone))
    );
}

#[tokio::test]
async fn docker_exact_restore_survives_provider_and_wrapper_replacement() {
    /* Live exact-restore table. C1 target absent; C2 provider wrapper is
     * dropped before aggregate CAS; C3 a fresh Docker client retries the exact
     * tuple; C4 the same effect carries another generation. Effects: E1 create
     * one target and retain its bind source; E2 C2+C3 recover the same handle;
     * E3 C4 fails without replacing it; E4 terminal disposal removes both
     * container and retained source. Rules D1=C1=>E1; D2=C2+C3=>E2;
     * D3=C4=>E3; D4=dispose=>E4. */
    let Some(first_runtime) = runtime().await else {
        return;
    };
    let scope = format!("docker-restore-{}", std::process::id());
    let _residue = residue::TestResidue::arm(&scope, residue::Backend::Docker);
    let spec = common::exact_restore_spec(&scope);
    let request = common::exact_restore_request(&scope);
    let authorization = disposal::disposal_authorization(&disposal::effect_fence(
        &scope,
        "restore-terminal",
        "terminal-owner",
        2,
    ));
    let first = ContainerProvider::new(Arc::new(first_runtime), "busybox:latest")
        .acquire_restore_environment(&spec, &request)
        .await
        .expect("D1 exact restore create");
    assert_eq!(
        first.disposition(),
        pc::SandboxRestoreTargetDisposition::Created,
        "D1/E1",
    );
    let handle = pc::Sandbox::handle(first.target().as_ref());
    let payload = handle
        .container_payload()
        .expect("D1/E1 typed container handle");
    let staging_root = match payload.runtime_handle.as_ref() {
        Some(awaken_sandbox_container::ContainerContinuationHandle::HostBindRestoration(
            locator,
        )) => std::path::PathBuf::from(locator.staging_root()),
        other => panic!("D1/E1 missing host staging evidence: {other:?}"),
    };
    drop(first);
    assert!(staging_root.is_dir(), "D2 retained after wrapper drop");

    let Some(retry_runtime) = runtime().await else {
        panic!("D2 daemon disappeared after exact target creation");
    };
    let retry = ContainerProvider::new(Arc::new(retry_runtime), "busybox:latest");
    let mut mismatch = request.clone();
    mismatch.generation_id.push_str("-other");
    assert!(
        retry
            .acquire_restore_environment(&spec, &mismatch)
            .await
            .is_err(),
        "D3/E3",
    );
    let recovered = retry
        .acquire_restore_environment(&spec, &request)
        .await
        .expect("D2 fresh provider recovery");
    assert_eq!(
        recovered.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "D2/E2",
    );
    assert_eq!(
        pc::Sandbox::handle(recovered.target().as_ref()),
        handle,
        "D2/E2"
    );
    drop(recovered);
    retry
        .dispose_restored_environment(
            &spec,
            &request,
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("D4 provider exact terminal cleanup");
    retry
        .dispose_restored_environment(
            &spec,
            &request,
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("D4 provider absent replay");
    assert!(!staging_root.exists(), "D4/E4");
}

#[tokio::test]
async fn docker_open_channel_dials_the_published_agent_port() {
    let Some(rt) = runtime().await else { return };
    let scope = format!("it-chan-{}", std::process::id());

    // Cause/effect rules: D1 a real container publishes an ephemeral Host port
    // -> the provider resolves it and the canonical factory dials that address;
    // D2 successful writes alone are insufficient -> require exact echo bytes;
    // D3 terminal success/failure -> remove only this test's container and
    // require physical absence. Cold readiness retries remain provider-owned.
    let container = residue::create_owned(
        &rt,
        &scope,
        &plan(&["sh", "-c", "while true; do nc -l -p 8080 -e cat; done"]),
        residue::Backend::Docker,
    )
    .await;
    let id = container.id();

    // The published port is discovered via inspect and dialed via ConnectionPlan.
    // Retry while the in-container listener binds.
    let mut opened = None;
    for _ in 0..25 {
        if let Ok(mut chan) = rt.open_channel(id).await {
            let exchange = async {
                chan.write_all(b"ping").await?;
                chan.flush().await?;
                let mut reply = [0_u8; 4];
                chan.read_exact(&mut reply).await?;
                Ok::<_, std::io::Error>(reply)
            };
            if matches!(tokio::time::timeout(Duration::from_secs(2), exchange).await,
                Ok(Ok(reply)) if &reply == b"ping")
            {
                opened = Some(());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    rt.remove(id)
        .await
        .expect("D3 remove this test's container");
    assert!(rt.inspect(id).await.is_err(), "D3 physical absence");
    assert!(
        opened.is_some(),
        "open_channel must reach the container's published agent port"
    );
}

#[tokio::test]
async fn docker_exec_can_be_polled_reattached_signaled_and_waited() {
    let Some(rt) = runtime().await else { return };
    let container = residue::create_owned(
        &rt,
        "it-exec",
        &plan(&["sleep", "300"]),
        residue::Backend::Docker,
    )
    .await;
    let id = container.id();

    let command = pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("printf established > {EXEC_ESTABLISHED_MARKER}; sleep 300"),
        ],
        cwd: "/tmp".into(),
        env: vec![pc::EnvVar {
            name: "EXEC_MARKER".into(),
            value: pc::EnvValue::Inline { value: "ok".into() },
            visibility: pc::EnvVisibility::Process,
        }],
        stdio: pc::Stdio::Null,
    };
    let command = pc::materialize_process_command(&[], command, None)
        .await
        .expect("materialize public exec environment");
    let process = rt
        .spawn(
            awaken_sandbox_container::AdmittedExec::unrecorded(id),
            command,
        )
        .await
        .expect("spawn detached exec");
    let public_id = process.id().to_string();

    // What this asserts is that a handle recovered by public id signals and
    // waits on the process that id names. The signal reaches that process
    // through a pid the exec wrapper records inside the container *after*
    // `spawn` has returned, so a signal racing that record fails for want of a
    // target rather than for want of the property — which is what made this
    // test fail under load. The exec's own first side effect is the proof the
    // record is already written, because the wrapper records the pid and only
    // then execs this command; the sleep behind it keeps the process alive
    // until the signal, so no outcome here is ambiguous.
    //
    // No port or event carries that record, so the marker file is its only
    // observable form and the deadline below fails loudly rather than letting
    // an exec that never started pass for one that did.
    let mut established = false;
    // awaken-wait: settles on the exec's own committed in-container marker.
    for _ in 0..150 {
        if container_holds(&rt, id, EXEC_ESTABLISHED_MARKER).await {
            established = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        established,
        "the detached exec never reached the command it was spawned for"
    );
    assert!(
        process.poll().await.unwrap().is_none(),
        "an established exec holds its sleep until something signals it"
    );

    let recovered = rt
        .process(id, &public_id)
        .await
        .expect("reattach the durable exec handle");
    recovered
        .signal(pc::Signal::Term)
        .await
        .expect("a reattached handle signals the live process it named");
    assert!(recovered.wait().await.unwrap().code.is_some());
    // Terminal-already is the other admitted outcome of signalling this handle,
    // and the adapter owes it success rather than an error. Asserting it here
    // is what lets the live signal above stay an unconditional expectation
    // instead of being softened into tolerating any failure.
    recovered
        .signal(pc::Signal::Term)
        .await
        .expect("signalling a terminal handle is a no-op, not a failure");
    assert!(rt.process("wrong-container", &public_id).await.is_err());
    process_lifecycle::verify_wait_cancellation_and_signal(&rt, id).await;

    let empty = pc::MaterializedCommand::new(Vec::<String>::new());
    assert!(
        rt.spawn(
            awaken_sandbox_container::AdmittedExec::unrecorded(id),
            empty.clone()
        )
        .await
        .is_err()
    );
    assert!(
        rt.spawn_agent(
            awaken_sandbox_container::AdmittedExec::unrecorded(id),
            empty
        )
        .await
        .is_err()
    );
}

/// Whether the container already holds `path` as a regular file, asked of the
/// container itself because that is the only observer of its own filesystem.
async fn container_holds(rt: &DockerRuntime, container_id: &str, path: &str) -> bool {
    let mut probe = pc::Command::new(["sh", "-c", &format!("test -f {path}")]);
    probe.cwd = "/tmp".into();
    probe.stdio = pc::Stdio::Null;
    let probe = pc::materialize_process_command(&[], probe, None)
        .await
        .expect("materialize the establishment probe");
    rt.spawn(
        awaken_sandbox_container::AdmittedExec::unrecorded(container_id),
        probe,
    )
    .await
    .expect("spawn the establishment probe")
    .wait()
    .await
    .expect("await the establishment probe")
    .code
        == Some(0)
}

#[tokio::test]
async fn reaping_stops_the_process_a_plain_spawn_started() {
    // The physical half of the exec-responsibility table. The in-crate table
    // uses a fake runtime, so it can only prove the reaper was *demanded*;
    // this proves a real daemon's reaper actually stops a process started
    // through the plain `spawn` edge — the edge whose attempts disposal used
    // to miss entirely — before anything reads the workspace.
    let Some(rt) = runtime().await else { return };
    let container = residue::create_owned(
        &rt,
        "it-reap",
        &plan(&["sleep", "300"]),
        residue::Backend::Docker,
    )
    .await;
    let id = container.id();

    let command = pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("printf established > {REAP_ESTABLISHED_MARKER}; sleep 300"),
        ],
        cwd: "/tmp".into(),
        env: Vec::new(),
        stdio: pc::Stdio::Null,
    };
    let command = pc::materialize_process_command(&[], command, None)
        .await
        .expect("materialize the reaped command");
    let process = rt
        .spawn(
            awaken_sandbox_container::AdmittedExec::unrecorded(id),
            command,
        )
        .await
        .expect("spawn detached exec");

    // Same establishment precondition as the signal test: the wrapper records
    // the pid before it execs this command, so the marker is proof the record
    // the reaper enumerates already exists.
    let mut established = false;
    // awaken-wait: settles on the exec's own committed in-container marker.
    for _ in 0..150 {
        if container_holds(&rt, id, REAP_ESTABLISHED_MARKER).await {
            established = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(established, "the reaped exec never reached its command");
    assert!(
        process.poll().await.unwrap().is_none(),
        "alive before reaping"
    );

    rt.reap_exec_orphans(id)
        .await
        .expect("the container forgets the execs it started");

    let stopped = tokio::time::timeout(Duration::from_secs(10), process.wait())
        .await
        .expect("reaping stops the process within its deadline")
        .expect("terminal status");
    assert!(
        stopped.code.is_some() || stopped.signaled,
        "the reaped process reached a terminal status: {stopped:?}"
    );
}
