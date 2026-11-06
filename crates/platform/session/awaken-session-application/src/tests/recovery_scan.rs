use super::*;

#[tokio::test]
async fn recovery_scan_reduces_pages_before_eof_and_discards_failed_prefixes() {
    // Cause/effect graph: C1 257 roots require two pages; C2 page two is held;
    // C3 page two succeeds, fails, stalls, or its read-only reducer fails;
    // C4 the caller is cancelled. Effects: E1 page one is reduced before the
    // second read; E2 no completed projection exists while that read is held;
    // E3 only successful EOF returns the complete count; E4 errors discard the
    // prefix; E5 cancellation owns/drops the pending scan, with no detached work.
    //
    // | Rule | Second page outcome | Completed result | Rows reduced |
    // | P1 | success | 257 | 257 |
    // | P2 | repository outage | error | 256 |
    // | P3 | non-advancing cursor | error | 256 |
    // | P4 | reducer failure | error | 256 |
    // | P5 | caller cancelled | cancelled | 256 |
    // The reducer retains a scalar only. Existing full-scan F2 and cutover V3
    // tests own the downstream zero-effect and unchanged-generation barriers.
    // Wait coverage: W1 every rule reaches the held second page; W2 P1-P4
    // finish after release; W3 P5 joins cancellation. A missing signal or
    // completion fails within two seconds with its rule and phase named,
    // rather than parking the suite or hiding which boundary failed.
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    for index in 0..257 {
        create(
            durable.as_ref(),
            persisted(&format!("fold-{index:03}"), true, "idle"),
        )
        .await;
    }
    for rule in ["P1", "P2", "P3", "P4", "P5"] {
        let repository = Arc::new(FaultingSessionRepository::new(durable.clone()));
        repository
            .hold_recovery_scan_on_call
            .store(2, Ordering::SeqCst);
        if rule == "P2" {
            repository.fail_recovery_scan_on_call(2);
        }
        if rule == "P3" {
            repository.stall_recovery_cursor_on_call(2);
        }
        let reduced = Arc::new(AtomicUsize::new(0));
        let scan_repository = repository.clone();
        let scan_reduced = reduced.clone();
        let task = tokio::spawn(async move {
            crate::recovery_scan::fold_reconcilable_sessions(
                scan_repository.as_ref(),
                0_usize,
                |count, page| {
                    let reduced = scan_reduced.clone();
                    async move {
                        if rule == "P4" && count != 0 {
                            return Err(awaken_session_contract::SessionRepositoryError::Corrupt(
                                "injected read-only reduction failure".into(),
                            ));
                        }
                        let count = count + page.sessions.len();
                        reduced.store(count, Ordering::SeqCst);
                        Ok(count)
                    }
                },
                std::convert::identity,
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            repository.recovery_scan_entered.notified(),
        )
        .await
        .unwrap_or_else(|_| panic!("{rule}/W1 second page must enter the hold"));
        assert_eq!(reduced.load(Ordering::SeqCst), 256, "{rule}/E1");
        assert!(!task.is_finished(), "{rule}/E2 no partial observation");
        if rule == "P5" {
            task.abort();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(2), task)
                    .await
                    .expect("P5/W3 cancellation must finish")
                    .unwrap_err()
                    .is_cancelled(),
                "P5/E5"
            );
            repository.recovery_scan_release.notify_one();
            tokio::task::yield_now().await;
            assert_eq!(
                repository.recovery_scan_count.load(Ordering::SeqCst),
                2,
                "P5/E5"
            );
        } else {
            repository.recovery_scan_release.notify_one();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .unwrap_or_else(|_| panic!("{rule}/W2 released scan must finish"))
                .unwrap();
            if rule == "P1" {
                let (count, quarantined) = result.expect("P1 EOF");
                assert_eq!(count, 257, "P1/E3");
                assert!(quarantined.is_empty(), "P1/E3");
            } else {
                assert!(result.is_err(), "{rule}/E4 failed prefix must not escape");
            }
        }
        assert_eq!(
            reduced.load(Ordering::SeqCst),
            if rule == "P1" { 257 } else { 256 },
            "{rule}"
        );
    }
}

#[tokio::test]
async fn work_recovery_reloads_current_truth_after_the_complete_identity_scan() {
    // Causes: C1 the scan observes an external live root; C2 its fresh read
    // observes terminal, local, missing, unavailable, or unchanged truth.
    // Effects: E1 terminal/local/missing cannot dispatch; E2 unavailable keeps
    // one exact retry diagnostic; E3 unchanged truth dispatches once. Rules:
    // W1=terminal/local/missing=>E1; W2=unavailable=>E1+E2; W3=unchanged=>E3.
    // No aggregate payload from the earlier scan may authorize the effect.
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    create(durable.as_ref(), persisted("fresh-work", true, "idle")).await;
    for rule in ["terminal", "local", "missing", "unavailable", "unchanged"] {
        let repository = FaultingSessionRepository::new(durable.clone());
        match rule {
            "terminal" => {
                *repository.get_override_once.lock().unwrap() =
                    Some(persisted("fresh-work", true, "terminated"))
            }
            "local" => {
                *repository.get_override_once.lock().unwrap() =
                    Some(persisted("fresh-work", false, "idle"))
            }
            "missing" => repository.get_not_found_once.store(true, Ordering::SeqCst),
            "unavailable" => {
                repository
                    .failed_gets
                    .lock()
                    .unwrap()
                    .insert("fresh-work".into());
            }
            _ => {}
        }
        let environments = RecordingEnvironmentSource::default();
        let report = reconcile_work_dispatches(&repository, &environments, 0).await;
        let expected = usize::from(rule == "unchanged");
        assert_eq!(report.pending, 1, "{rule}: complete scan identity count");
        assert_eq!(report.settled, expected, "{rule}");
        assert_eq!(
            environments.dispatch_calls.load(Ordering::SeqCst),
            expected,
            "{rule}"
        );
        assert_eq!(
            report.failures.len(),
            usize::from(rule == "unavailable"),
            "{rule}"
        );
        if rule == "unavailable" {
            assert_eq!(
                report.failures[0].session_id, "fresh-work",
                "W2 exact diagnostic"
            );
            assert!(
                report.failures[0].environment_id.is_empty(),
                "W2 no stale Environment"
            );
        }
    }
}
