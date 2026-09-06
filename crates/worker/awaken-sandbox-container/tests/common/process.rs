//! One real attached-exec lifecycle oracle for Docker, Podman and Kubernetes.
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerRuntime;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(crate) async fn verify_wait_cancellation_and_signal(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
) {
    // KRP2 causes: the exec acknowledges an installed TERM trap; one actual
    // wait is polled then cancelled; another wait remains pending while signal
    // performs its preflight poll. Effects: no lost completion or lifetime lock,
    // TERM reaches the owned process, both wait/poll retain its exact exit 7.
    // Readiness is a process-produced marker, not a guessed startup delay. Each caller
    // reuses its existing real container fixture and production disposal.
    let stopped = runtime
        .spawn_agent(
            container_id,
            pc::MaterializedCommand::new([
                "sh",
                "-c",
                "trap 'exit 7' TERM; printf ready; read line",
            ]),
        )
        .await
        .expect("KRP2 attached exec with TERM trap");
    let mut channel = stopped.channel;
    let mut ready = [0; 5];
    tokio::time::timeout(Duration::from_secs(10), channel.read_exact(&mut ready))
        .await
        .expect("KRP2 readiness deadline")
        .expect("KRP2 ready bytes");
    assert_eq!(&ready, b"ready");
    let mut abandoned = Box::pin(stopped.process.wait());
    std::future::poll_fn(|cx| {
        assert!(
            abandoned.as_mut().poll(cx).is_pending(),
            "KRP2 first wait entered"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stopped.process.poll())
            .await
            .expect("KRP2 concurrent poll does not wait for exit")
            .unwrap(),
        None
    );
    drop(abandoned);
    assert_eq!(
        stopped.process.poll().await.unwrap(),
        None,
        "KRP2 cancelled observer retains live completion"
    );
    let mut waiting = Box::pin(stopped.process.wait());
    std::future::poll_fn(|cx| {
        assert!(
            waiting.as_mut().poll(cx).is_pending(),
            "KRP2 replacement wait entered"
        );
        std::task::Poll::Ready(())
    })
    .await;
    let (status, signal) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(waiting, stopped.process.signal(pc::Signal::Term))
    })
    .await
    .expect("KRP2 signal and wait cannot deadlock");
    signal.expect("KRP2 delivered TERM");
    assert_eq!(status.unwrap().code, Some(7), "KRP2 actual trap exit");
    assert_eq!(
        stopped.process.poll().await.unwrap().unwrap().code,
        Some(7),
        "KRP2 repeat exact status"
    );
}
