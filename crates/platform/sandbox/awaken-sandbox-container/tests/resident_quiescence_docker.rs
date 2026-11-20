//! Real namespace/process proof using an explicitly selected immutable Hand
//! image. This is Docker evidence; ACK/PVC/object custody are separate gates.
#![cfg(all(target_os = "linux", feature = "docker"))]
#[path = "common/residue.rs"]
mod residue;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{Sandbox as _, SessionSandbox as _};
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{
    ContainerHandleCodec as _, ContainerProvider, ContainerRuntime, ResidentHandConfig,
};
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncReadExt as _;

#[tokio::test]
async fn resident_source_freezes_replays_and_keeps_bytes_before_owned_disposal() {
    // P0 absent optional image -> explicit skip; configured image/daemon
    // failure -> test failure. P1 an immutable older resident Hand plus
    // source filesystem bytes. P2 resident Hand + live unregistered writer ->
    // provider stops both execution paths and independently observes exit.
    // P3 repeat -> same source/bytes; Session spawn refused, maintenance read
    // allowed behind sealed admission. P4 owning disposal -> exact ID gone.
    let Some(image) = std::env::var_os("AWAKEN_TEST_RESIDENT_HAND_IMAGE") else {
        eprintln!(
            "SKIPPED resident process fixture: AWAKEN_TEST_RESIDENT_HAND_IMAGE is not configured"
        );
        return;
    };
    assert!(
        std::process::Command::new("docker")
            .arg("version")
            .output()
            .is_ok_and(|o| o.status.success()),
        "configured Docker fixture must be reachable"
    );
    let scope = format!("resident-quiescence-{}", uuid::Uuid::new_v4().simple());
    let _residue = residue::TestResidue::arm(&scope, residue::Backend::Docker);
    tokio::time::timeout(Duration::from_secs(90), async {
        let runtime = Arc::new(DockerRuntime::connect_local(7777).unwrap());
        let provider = ContainerProvider::new(runtime.clone(), image.to_string_lossy())
            .with_clock(awaken_time::system_clock())
            .with_resident_hand(
                ResidentHandConfig::new("/usr/local/bin/awaken-sandbox", 7777).unwrap(),
            );
        let mut spec = awaken_sandbox_container::test_support::spec(&scope);
        spec.mounts.clear();
        spec.env.clear();
        spec.command.clear();
        spec.network = pc::NetworkPolicy::Unrestricted;
        let sandbox = provider
            .create_container(&spec)
            .await
            .expect("configured Hand image must create a hardened source");
        let original = sandbox.handle();
        let cid = original.container_payload().unwrap().container_id;
        let written = sandbox
            .spawn(pc::Command::new([
                "sh",
                "-ec",
                "printf x >> /workspace/count; printf saved > /workspace/keep",
            ]))
            .await
            .unwrap();
        assert_eq!(written.wait().await.unwrap().code, Some(0));
        let rogue = sandbox
            .spawn(pc::Command::new([
                "sh",
                "-c",
                "trap '' TERM; while :; do sleep 1; done",
            ]))
            .await
            .unwrap();
        sandbox
            .quiesce_resident()
            .await
            .expect("P2 physical quiescence");
        tokio::time::timeout(Duration::from_secs(5), rogue.wait())
            .await
            .expect("P2 observed actual exit")
            .unwrap();
        assert!(
            sandbox
                .spawn(pc::Command::new(["sh", "-c", "touch /workspace/late"]))
                .await
                .is_err(),
            "P3 Session admission remains closed"
        );
        let pc::AgentProcess {
            process,
            mut channel,
        } = sandbox
            .spawn_agent_process(
                pc::SessionProcessPurpose::Maintenance,
                pc::Command::new([
                    "sh",
                    "-c",
                    "test ! -e /workspace/late && cat /workspace/count /workspace/keep",
                ]),
            )
            .await
            .unwrap();
        let mut bytes = Vec::new();
        channel.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(process.wait().await.unwrap().code, Some(0));
        assert_eq!(bytes, b"xsaved", "P1/P3 source filesystem is preserved");
        sandbox.quiesce_resident().await.unwrap();
        assert_eq!(sandbox.handle(), original, "P3 original immutable source");
        sandbox.dispose_unattached().await.unwrap();
        assert!(
            matches!(
                runtime.inspect(&cid).await,
                Err(awaken_container_contract::RuntimeError::NotFound(_))
            ),
            "P4 exact engine 404"
        );
    })
    .await
    .expect("the real fixture has one bounded lifetime");
}
