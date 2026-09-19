//! One Worker restart against a real container daemon, shared by the Docker
//! and Podman suites (warm-capacity lease rules L1/L2/L3/L6 in
//! `awaken_sandbox_container::pool::lease`).
//!
//! Worker A runs on its own Tokio runtime. "Process death" is dropping that
//! runtime after `std::mem::forget`-ing A's pool and its checked-out
//! Environment: every task A spawned (lease maintenance, replenishment) is
//! cancelled wherever it stood, no `shutdown` runs, and no destructor of A's
//! values runs — exactly what a SIGKILL leaves behind, minus the process exit.
//! Worker B is a new incarnation on a new runtime over the same daemon, lease
//! directory and realization namespace. Lease time is a manual clock, so
//! "after expiry" is a clock value, not a wait.

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::pool::{WarmCapacityLeasePolicy, WarmCapacityLeases, WarmLeaseClock};
use awaken_sandbox_container::{
    ContainerEnvironmentProvider, ContainerProvider, ContainerRealizationNamespace,
    ContainerRuntime, DecodedContainerHandle, WarmContainerPool,
};

struct ManualClock(AtomicU64);

impl WarmLeaseClock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec!["sh".into(), "-c".into(), "sleep 3600".into()],
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        // The one poolable row: mount-less and Ephemeral.
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        lease_ttl_secs: None,
        control_services: Default::default(),
    }
}

/// The label this run's private image carries, and therefore every container
/// created from it. `ancestor=` would not do: it matches by image *id*, so any
/// other suite's container from the same base image would be counted too.
const RUN_LABEL: &str = "awaken.warm-lease-it";

/// Removes every container carrying this run's label, and its private image,
/// however the test ends.
struct DaemonCleanup {
    cli: &'static str,
    tag: String,
    nonce: String,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        for id in containers(self.cli, &self.nonce) {
            let _ = Command::new(self.cli).args(["rm", "-f", &id]).output();
        }
        let _ = Command::new(self.cli).args(["rmi", &self.tag]).output();
    }
}

/// Build this run's private image: the base image plus one label, committed
/// locally (no registry access).
fn private_image(cli: &str, base_image: &str, tag: &str, nonce: &str) {
    let seed = Command::new(cli)
        .args(["create", base_image, "true"])
        .output()
        .expect("create seed container");
    assert!(seed.status.success(), "{cli} create {base_image}");
    let seed = String::from_utf8_lossy(&seed.stdout).trim().to_owned();
    let committed = Command::new(cli)
        .args([
            "commit",
            "--change",
            &format!("LABEL {RUN_LABEL}={nonce}"),
            &seed,
            tag,
        ])
        .output()
        .expect("commit private image");
    let _ = Command::new(cli).args(["rm", "-f", &seed]).output();
    assert!(
        committed.status.success(),
        "{cli} commit: {}",
        String::from_utf8_lossy(&committed.stderr)
    );
}

/// Every container (running or not) of this run, by full id.
fn containers(cli: &str, nonce: &str) -> BTreeSet<String> {
    let output = Command::new(cli)
        .args([
            "ps",
            "-a",
            "--no-trunc",
            "--filter",
            &format!("label={RUN_LABEL}={nonce}"),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("list containers");
    assert!(output.status.success(), "{cli} ps failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn lease_records(root: &std::path::Path) -> Vec<String> {
    let mut names = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|domain| std::fs::read_dir(domain.path()).into_iter().flatten())
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

pub(crate) fn a_restarted_worker_reclaims_only_expired_never_checked_out_capacity<R, F>(
    cli: &'static str,
    base_image: &str,
    make_runtime: F,
) where
    R: ContainerRuntime + 'static,
    F: Fn(ContainerRealizationNamespace) -> R,
{
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let tag = format!("localhost/awaken-warmlease-it-{nonce}:latest");
    let _cleanup = DaemonCleanup {
        cli,
        tag: tag.clone(),
        nonce: nonce.clone(),
    };
    private_image(cli, base_image, &tag, &nonce);
    let root = tempfile::tempdir().expect("lease root");
    let policy = WarmCapacityLeasePolicy::default();
    let ttl = u64::try_from(policy.ttl().as_millis()).unwrap();
    let t0 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let clock = Arc::new(ManualClock(AtomicU64::new(t0)));
    let namespace =
        ContainerRealizationNamespace::from_stable_parts(["warm-lease-restart-it", nonce.as_str()])
            .unwrap();
    let leases = || {
        WarmCapacityLeases::new(root.path(), policy)
            .with_clock(clock.clone() as Arc<dyn WarmLeaseClock>)
    };

    // Worker A: prewarm two, check one out to a Session, then die.
    let worker_a = current_thread_runtime();
    let checked_out = worker_a.block_on(async {
        let pool = WarmContainerPool::new(
            Arc::new(ContainerProvider::new(
                Arc::new(make_runtime(namespace.clone())),
                tag.clone(),
            )),
            2,
        )
        .with_capacity_leases(leases())
        .await
        .expect("worker A leases its warm capacity");
        assert_eq!(pool.prewarm(&spec("warm"), 2).await.expect("prewarm"), 2);
        let environment = pool
            .create_unattached_environment(&spec("session"))
            .await
            .expect("check out one warm container");
        let id = DecodedContainerHandle::decode(&pc::Sandbox::handle(environment.as_ref()))
            .expect("container handle")
            .payload
            .container_id;
        std::mem::forget(environment);
        std::mem::forget(pool);
        id
    });
    drop(worker_a);

    let before = containers(cli, &nonce);
    assert_eq!(
        before.len(),
        2,
        "one warm + one checked out: {before:?}, records {:?}",
        lease_records(root.path())
    );
    assert!(before.contains(&checked_out), "{checked_out} in {before:?}");
    let records = lease_records(root.path());
    assert_eq!(
        records
            .iter()
            .filter(|name| name.ends_with(".lease"))
            .count(),
        1,
        "L3 the checkout consumed its record; the warm one keeps its own: {records:?}"
    );

    // Worker B: a new incarnation over the same daemon and lease directory.
    let worker_b = current_thread_runtime();
    worker_b.block_on(async {
        let pool = WarmContainerPool::new(
            Arc::new(ContainerProvider::new(
                Arc::new(make_runtime(namespace.clone())),
                tag.clone(),
            )),
            2,
        )
        .with_capacity_leases(leases().without_maintenance())
        .await
        .expect("worker B leases its warm capacity");
        assert_eq!(
            containers(cli, &nonce),
            before,
            "L1 startup reclamation leaves a live lease alone"
        );

        clock.0.store(t0 + ttl - 1, Ordering::SeqCst);
        let early = pool.reclaim_expired_capacity().await.unwrap();
        assert_eq!(
            early,
            Default::default(),
            "L1 a live record is neither claimed nor even briefly taken"
        );
        assert_eq!(containers(cli, &nonce), before, "L1 one millisecond early");

        clock.0.store(t0 + ttl, Ordering::SeqCst);
        let due = pool.reclaim_expired_capacity().await.unwrap();
        assert_eq!(due.reclaimed.len(), 1, "L2 {due:?}");
        assert!(due.failed.is_empty(), "L2 {due:?}");
        assert_eq!(
            containers(cli, &nonce),
            BTreeSet::from([checked_out.clone()]),
            "L2 the dead Worker's warm container is gone; L3 the Session's survives"
        );
        assert_eq!(
            pool.reclaim_expired_capacity().await.unwrap(),
            Default::default(),
            "L2 exactly once"
        );
        assert!(
            lease_records(root.path()).is_empty(),
            "L2/L6 the claim is deleted after removal"
        );
        pool.shutdown().await;
    });
    eprintln!(
        "{cli}: worker A left {} containers; worker B reclaimed 1 after expiry and kept the checked-out {checked_out}",
        before.len()
    );
}
