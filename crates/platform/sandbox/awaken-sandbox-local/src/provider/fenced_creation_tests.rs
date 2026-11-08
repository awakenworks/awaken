//! A fenced creation keeps the caller's async worker serving other tasks while
//! its durable writes run under the borrowed authority.

use super::workdir_helper_tests::workdir_spec;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// How long the parked admission waits for the other task before giving up.
/// Only an inline creation, which holds the worker that task needs, reaches it.
const PARK_LIMIT: Duration = Duration::from_secs(20);

/// Admits exactly like the shared fixture, but parks inside the first admitted
/// effect, the creation marker's durable write, until the other task reports
/// that it ran, and records whether it did before the park gave up.
struct ParkedAuthority {
    admits: pc::test_support::EffectAuthority,
    other_task: awaken_sync::LeafLock<mpsc::Receiver<()>>,
    parked: AtomicBool,
    other_task_ran_while_parked: AtomicBool,
}

impl pc::CurrentExecutionAuthority for ParkedAuthority {
    fn with_current(
        &self,
        effect: &mut dyn FnMut(Duration) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.admits.with_current(effect)
    }

    fn with_current_effect(
        &self,
        asserted: &pc::SandboxEffectFence,
        effect: &mut dyn FnMut(&pc::SandboxEffectFence, Duration) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.admits
            .with_current_effect(asserted, &mut |fence, remaining| {
                if !self.parked.swap(true, Ordering::SeqCst) {
                    let ran = self.other_task.lock().recv_timeout(PARK_LIMIT).is_ok();
                    self.other_task_ran_while_parked
                        .store(ran, Ordering::SeqCst);
                }
                effect(fence, remaining)
            })
    }
}

#[test]
fn a_parked_fenced_creation_hands_its_async_worker_to_other_tasks() {
    // Decision table. C1 a fenced Workdir creation without mounts runs in a
    // task on a runtime with one async worker; C2 its first admitted durable
    // write is parked inside the authority's critical section; C3 another task
    // queued on that worker is waiting to run, as a lease heartbeat is when it
    // comes due. E1 that task runs while the write is parked; E2 the creation
    // still completes under the same fence: the sandbox is Ready with the
    // spec's realization fingerprint. Inline, the parked write holds the only
    // worker and the other task runs only after the park gives up.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let base = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(base.path());
    let spec = workdir_spec("parked-fenced-creation", false);
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec);
    let fence = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let (beat, other_task) = mpsc::channel();
    let authority = Arc::new(ParkedAuthority {
        admits: pc::test_support::EffectAuthority::new(&fence),
        other_task: awaken_sync::LeafLock::new(other_task),
        parked: AtomicBool::new(false),
        other_task_ran_while_parked: AtomicBool::new(false),
    });

    let created = runtime.block_on({
        let authority = authority.clone();
        async move {
            tokio::spawn(async move {
                let other = tokio::spawn(async move {
                    let _ = beat.send(());
                });
                let created = provider
                    .create_sandbox_for_effect(&spec, &fence, None, &*authority)
                    .await;
                other.await.unwrap();
                created
            })
            .await
            .unwrap()
        }
    });

    assert!(authority.parked.load(Ordering::SeqCst), "C2");
    assert!(
        authority.other_task_ran_while_parked.load(Ordering::SeqCst),
        "E1"
    );
    let created = created.expect("E2");
    assert_eq!(
        pc::Sandbox::handle(&created).realization_fingerprint(),
        Some(&fingerprint),
        "E2"
    );
}
