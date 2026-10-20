//! Persistent Bash framing and process-lifetime regression tests.
#![cfg(test)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::*;

#[test]
fn bash_completion_requires_the_whole_status_line() {
    // F1 graph: marker/status/newline can each span reads. Every proper
    // prefix is nonterminal; only the full frame yields the exact status.
    // F2 malformed/overflowing status is never converted to a fake -1.
    for code in [0, 1, 127, 255] {
        let frame = format!("output\n__marker__{code}\n");
        for end in 0..frame.len() {
            assert_eq!(
                bash_completion(&frame.as_bytes()[..end], "__marker__"),
                None
            );
        }
        assert_eq!(
            bash_completion(frame.as_bytes(), "__marker__"),
            Some((7, code))
        );
    }
    for invalid in ["", "abc", "99999999999999999999999999"] {
        assert_eq!(
            bash_completion(format!("__marker__{invalid}\n").as_bytes(), "__marker__"),
            None
        );
    }
}

/// Timeout bound. T1 absent/zero => default; T2 at the maximum => accepted;
/// T3 above the maximum => InvalidArguments naming the bound, and no shell
/// is started for it (the next call still runs normally). The schema
/// publishes the bound so the model sees it before calling.
#[tokio::test]
async fn model_timeout_is_bounded_and_the_bound_is_published() {
    let schema = serde_json::to_value(schemars::schema_for!(BashArgs)).unwrap();
    assert_eq!(
        schema["properties"]["timeout_ms"]["maximum"],
        serde_json::json!(BASH_MAX_TIMEOUT_MS),
        "published bound: {schema}"
    );
    let tool = BashTool::new(&HandToolContext::default());
    let call = |timeout_ms| BashArgs {
        command: "printf ok".into(),
        restart: false,
        timeout_ms,
    };
    let error = tool
        .call(call(Some(BASH_MAX_TIMEOUT_MS + 1)))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ToolError::InvalidArguments(message) if message.contains("600000")),
        "T3 {error:?}"
    );
    assert!(tool.session.lock().await.is_none(), "T3 no shell started");
    for timeout in [None, Some(0), Some(BASH_MAX_TIMEOUT_MS)] {
        assert_eq!(
            tool.call(call(timeout)).await.unwrap(),
            "ok",
            "T1/T2 {timeout:?}"
        );
    }
}

/// F1 provider rejects launcher admission -> exact definitive error, no
/// preparation or process effects. F2 admitted executable but preparation
/// fails -> preparation error, no executable/workload effects. F3 restart
/// does not bypass either refusal; no shell state is published in any row.
#[tokio::test]
async fn refused_or_unprepared_launcher_never_starts_a_shell() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("workload-ran");
    let prepared = Arc::new(AtomicBool::new(false));
    let hook = prepared.clone();
    let refused = HandToolContext::new(directory.path())
        .with_bash_launcher_failure("isolation launcher unavailable")
        .with_bash_launch_preparation(Arc::new(move |_| {
            hook.store(true, Ordering::SeqCst);
            Ok(())
        }));
    let unprepared = HandToolContext::new(directory.path())
        .with_bash_launcher("/bin/bash", [])
        .with_bash_launch_preparation(Arc::new(|_| {
            Err(std::io::Error::other("syscall profile unavailable"))
        }));
    for (context, expected) in [
        (refused, "isolation launcher unavailable"),
        (unprepared, "syscall profile unavailable"),
    ] {
        let context = context.with_bash_env(std::collections::BTreeMap::from([(
            "PROBE_MARKER".into(),
            marker.to_string_lossy().into_owned(),
        )]));
        let tool = BashTool::new(&context);
        for restart in [false, true] {
            let error = tool
                .call(BashArgs {
                    command: "printf attacked > \"$PROBE_MARKER\"".into(),
                    restart,
                    timeout_ms: None,
                })
                .await
                .unwrap_err();
            assert!(
                matches!(error, ToolError::DefinitiveFailure(ref cause) if cause.contains(expected)),
                "F1-F3 {error:?}"
            );
            assert!(tool.session.lock().await.is_none(), "F3 no shell published");
            assert!(!marker.exists(), "F1/F2 workload never executed");
        }
    }
    assert!(!prepared.load(Ordering::SeqCst), "F1 before preparation");
}

#[tokio::test]
async fn closed_stderr_does_not_discard_live_stdout_or_shell_state() {
    // E1 stderr closes before any reply, stdout stays live -> complete
    // output/status, not shell termination. E2 the same shell still owns
    // exported state on the next invocation; no hidden restart occurs.
    let context = HandToolContext::default().with_bash_launcher(
        persistent_bash_command().as_std().get_program().to_owned(),
        [
            "-c".into(),
            "exec 2>&-; exec /bin/bash --noprofile --norc".into(),
        ],
    );
    let tool = BashTool::new(&context);
    for command in [
        "export PIPE_STATE=kept; printf first",
        "printf '%s' \"$PIPE_STATE\"",
    ] {
        let output = tool
            .call(BashArgs {
                command: command.into(),
                restart: false,
                timeout_ms: Some(1000),
            })
            .await
            .unwrap();
        assert_eq!(
            output,
            if command.starts_with("export") {
                "first"
            } else {
                "kept"
            }
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bash_wait_does_not_starve_runtime_control_tasks() {
    // A long local command must yield the sole async runtime thread. Worker
    // heartbeat and dispatch renewal use the same scheduling path in the
    // served runtime, so blocking here previously expired both authorities.
    let control_task_ran = Arc::new(AtomicBool::new(false));
    let observed = control_task_ran.clone();
    let control = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        observed.store(true, Ordering::SeqCst);
    });

    BashTool::new(&HandToolContext::default())
        .call(BashArgs {
            command: "sleep 0.15".into(),
            restart: false,
            timeout_ms: None,
        })
        .await
        .expect("sleep command");

    assert!(control_task_ran.load(Ordering::SeqCst));
    control.await.expect("control task");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_bash_call_kills_its_descendant_process_group() {
    // Cancellation previously dropped only the blocking-task join handle,
    // leaving the shell and descendants alive beneath the Worker service.
    // A descendant reports readiness, would write `leaked` after cancellation,
    // and is required to disappear with its process group instead.
    let directory = tempfile::tempdir().expect("temporary marker directory");
    let ready = directory.path().join("ready");
    let leaked = directory.path().join("leaked");
    let command = format!(
        "sh -c 'printf ready > {}; sleep 0.4; printf leaked > {}' & wait",
        ready.display(),
        leaked.display()
    );
    let tool = BashTool::new(&HandToolContext::new(directory.path()));
    let call = tokio::spawn(async move {
        tool.call(BashArgs {
            command,
            restart: false,
            timeout_ms: None,
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !ready.is_file() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant becomes ready");

    call.abort();
    let _ = call.await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(
        !leaked.exists(),
        "a descendant survived cancellation and wrote {}",
        leaked.display()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_blocked_command_write_obeys_cancellation_and_timeout() {
    // W1 launcher is live but has not read stdin; a command larger than the
    // pipe capacity blocks in write_all. W2 cancellation/timeout -> existing
    // process-group guard stops the launcher and descendants before their
    // delayed effect. W3 the same tool accepts a distinct next invocation;
    // the cancelled command is never replayed. Read-phase cancellation and
    // background-job preservation are covered by adjacent existing tests.
    for timeout in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let context = HandToolContext::new(directory.path()).with_bash_launcher(
                "/bin/bash", ["-c".into(), "if [ ! -e ready ]; then printf ready > ready; sleep 0.5; printf leaked > leaked; fi; exec /bin/bash --noprofile --norc".into()],
            );
        let tool = Arc::new(BashTool::new(&context));
        let pending = {
            let tool = tool.clone();
            tokio::spawn(async move {
                tool.call(BashArgs {
                    command: format!("# {}\ntouch replayed", "x".repeat(2 * 1024 * 1024)),
                    restart: false,
                    timeout_ms: Some(if timeout { 100 } else { 2000 }),
                })
                .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !directory.path().join("ready").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if timeout {
            let error = pending.await.unwrap().unwrap_err();
            assert!(error.to_string().contains("timed out after 100ms"));
        } else {
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
        }
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(!directory.path().join("leaked").exists());
        assert!(!directory.path().join("replayed").exists());
        let next = tool
            .call(BashArgs {
                command: "printf next".into(),
                restart: false,
                timeout_ms: Some(1000),
            })
            .await
            .unwrap();
        assert_eq!(next, "next");
    }
}

#[tokio::test]
async fn completed_bash_call_keeps_background_jobs_until_restart() {
    let directory = tempfile::tempdir().expect("temporary marker directory");
    let leaked = directory.path().join("leaked");
    let tool = BashTool::new(&HandToolContext::new(directory.path()));
    // The child cannot finish until the foreground call has returned and
    // this test releases it. Waiting for pipe EOF therefore fails without
    // depending on a race between two wall-clock delays.
    let command = concat!(
        "sh -c 'while [ ! -e released ]; do sleep 0.01; done; ",
        "printf survived > leaked' & printf done",
    )
    .to_owned();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.call(BashArgs {
            command,
            restart: false,
            timeout_ms: None,
        }),
    )
    .await
    .expect("an unredirected background child must not hold the tool pipe open")
    .expect("foreground shell succeeds");
    assert_eq!(output, "done");
    std::fs::write(directory.path().join("released"), b"release")
        .expect("release only this fixture's background child");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !leaked.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("persistent Bash must preserve background jobs across calls");
    assert_eq!(std::fs::read(&leaked).unwrap(), b"survived");
    tool.call(BashArgs {
        command: String::new(),
        restart: true,
        timeout_ms: None,
    })
    .await
    .expect("restart closes the old process group");
}
