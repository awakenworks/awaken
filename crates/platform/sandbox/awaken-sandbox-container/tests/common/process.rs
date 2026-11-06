//! One real attached-exec lifecycle oracle for Docker, Podman and Kubernetes.
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerRuntime;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(crate) async fn verify_security_floor(runtime: &dyn ContainerRuntime, container_id: &str) {
    // SF1 actual exec UID/GID matches the production image identity; SF2 all
    // effective capabilities are gone; SF3 NNP and seccomp filtering are set;
    // SF4 root mount is read-only while /tmp and outputs remain writable.
    // Exact nonzero row codes expose which effect failed. Rendered flags or
    // Docker/Podman inspect metadata alone do not prove these kernel effects.
    let script = r#"
test "$(id -u)" = 10001 || exit 41
test "$(id -g)" = 10001 || exit 42
grep -Eq '^CapEff:[[:space:]]*0+$' /proc/self/status || exit 43
grep -Eq '^NoNewPrivs:[[:space:]]*1$' /proc/self/status || exit 44
grep -Eq '^Seccomp:[[:space:]]*2$' /proc/self/status || exit 45
options=$(awk '$5 == "/" { print $6 }' /proc/self/mountinfo)
case ",$options," in *,ro,*) ;; *) exit 46 ;; esac
printf floor > /tmp/awaken-floor && printf floor > /mnt/session/outputs/awaken-floor || exit 47
rm /tmp/awaken-floor /mnt/session/outputs/awaken-floor || exit 48
"#;
    let process = runtime
        .spawn(
            awaken_sandbox_container::AdmittedExec::unrecorded(container_id),
            pc::MaterializedCommand::new(["sh", "-c", script]),
        )
        .await
        .expect("SF actual security probe");
    let status = tokio::time::timeout(Duration::from_secs(10), process.wait())
        .await
        .expect("SF probe deadline")
        .expect("SF probe exit");
    assert_eq!(
        status.code,
        Some(0),
        "SF1-SF4 real kernel security floor (41-48 identify cause)"
    );
}

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
    // KRP3 adds TERM ignored -> KILL, two simultaneous waiters and late signal.
    // KRP2/KRP3 share the same real process owner and cancellation schedule.
    for (script, expected, force_kill) in [
        ("trap 'exit 7' TERM; printf ready; read line", 7, false),
        ("trap '' TERM; printf ready; read line", 137, true),
    ] {
        let stopped = runtime
            .spawn_agent(
                awaken_sandbox_container::AdmittedExec::unrecorded(container_id),
                pc::MaterializedCommand::new(["sh", "-c", script]),
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
        let signal = async {
            stopped.process.signal(pc::Signal::Term).await?;
            if force_kill {
                assert!(
                    stopped.process.poll().await?.is_none(),
                    "KRP3 TERM did not prove exit"
                );
                stopped.process.signal(pc::Signal::Kill).await?;
            }
            Ok::<(), pc::SandboxError>(())
        };
        let (status, second, signal) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(waiting, stopped.process.wait(), signal)
        })
        .await
        .expect("KRP2 signal and wait cannot deadlock");
        signal.expect("KRP2 delivered TERM");
        assert_eq!(status.unwrap().code, Some(expected), "actual process exit");
        assert_eq!(
            second.unwrap().code,
            Some(expected),
            "second waiter sees same exit"
        );
        stopped
            .process
            .signal(pc::Signal::Term)
            .await
            .expect("late signal is idempotent");
        assert_eq!(stopped.process.wait().await.unwrap().code, Some(expected));
        assert_eq!(
            stopped.process.poll().await.unwrap().unwrap().code,
            Some(expected),
            "KRP2 repeat exact status"
        );
    }
}
