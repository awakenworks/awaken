//! Deployment schedule calculation and due-run orchestration.
//!
//! This module owns no state. It operates on the one [`DeploymentApplication`]
//! aggregate and its existing repository/launcher seams.

use awaken_deployment_contract::{Cron, MAX_DEPLOYMENT_REVISION, ScheduledRunClaimOutcome};
use chrono_tz::Tz;

use super::{
    DeploymentApplication, DeploymentApplicationError, DeploymentLaunch, DeploymentRecord,
    DeploymentRunRecord, DeploymentRunView, DeploymentSchedule, DeploymentStatus,
    DeploymentTrigger, launch_for, lifecycle_fact, next_revision, stored_deployment, stored_run,
    timestamp,
};

const MIN_JITTER_BOUND_MS: u64 = 5_000;
pub(super) const MAX_JITTER_BOUND_MS: u64 = 9 * 60_000;

type ScheduledCandidate = (
    String,
    DeploymentLaunch,
    DeploymentRunRecord,
    DeploymentRecord,
    u64,
);

pub(super) fn validate_schedule(
    schedule: Option<&DeploymentSchedule>,
) -> Result<(), DeploymentApplicationError> {
    let Some(schedule) = schedule else {
        return Ok(());
    };
    Cron::parse(schedule.expression()).map_err(|error| {
        DeploymentApplicationError::Invalid(format!("invalid cron schedule: {error}"))
    })?;
    schedule.timezone().parse::<Tz>().map_err(|error| {
        DeploymentApplicationError::Invalid(format!("invalid IANA timezone: {error}"))
    })?;
    Ok(())
}

fn parsed_schedule(schedule: &DeploymentSchedule) -> Option<(Cron, Tz)> {
    Some((
        Cron::parse(schedule.expression()).ok()?,
        schedule.timezone().parse().ok()?,
    ))
}

pub(super) fn next_occurrence(schedule: &DeploymentSchedule, after_ms: u64) -> Option<u64> {
    let (cron, timezone) = parsed_schedule(schedule)?;
    cron.next_after_in(after_ms, timezone)
}

fn active_cron(record: &DeploymentRecord) -> Option<(Cron, Tz)> {
    if record.status != DeploymentStatus::Active || record.archived_at.is_some() {
        return None;
    }
    parsed_schedule(record.schedule.as_ref()?)
}

fn execution_jitter_bound_ms(interval_ms: u64) -> u64 {
    interval_ms
        .saturating_mul(15)
        .checked_div(100)
        .unwrap_or_default()
        .clamp(MIN_JITTER_BOUND_MS, MAX_JITTER_BOUND_MS)
}

pub(super) fn execution_jitter_ms(deployment_id: &str, scheduled_ms: u64, interval_ms: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in deployment_id.bytes().chain(scheduled_ms.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % (execution_jitter_bound_ms(interval_ms) + 1)
}

impl DeploymentApplication {
    pub(super) async fn tick_and_launch_scheduled(
        &self,
        now: u64,
    ) -> Result<Vec<DeploymentRunView>, DeploymentApplicationError> {
        self.refresh().await?;
        self.refresh_executable_projections().await?;
        let candidates = self.tick(now)?;
        let mut completed = Vec::with_capacity(candidates.len());
        for (run_id, launch, run, deployment, expected_revision) in candidates {
            if self
                .primary_agent_missing(&launch.workspace_id, &launch.agent.id)
                .await?
            {
                self.runs
                    .lock()
                    .expect("DeploymentRun projection lock")
                    .remove(&run_id);
                // `tick` speculatively advances the working cursor. Restore the
                // durable revision before the Agent cascade performs its CAS.
                self.refresh().await?;
                self.archive_for_agent(&launch.workspace_id, &launch.agent.id)
                    .await?;
                continue;
            }
            if let Some(repository) = &self.repository {
                let DeploymentTrigger::Schedule { scheduled_at } = &run.trigger else {
                    return Err(DeploymentApplicationError::Invalid(
                        "scheduler produced a non-scheduled run".into(),
                    ));
                };
                let claim_id = format!("{}:{scheduled_at}", run.deployment_id);
                match repository
                    .claim_scheduled_run(
                        &claim_id,
                        expected_revision,
                        stored_deployment(&run.deployment_id, &deployment)?,
                        stored_run(&run_id, &run)?,
                        lifecycle_fact(
                            format!("deployment_run:{run_id}:deployment_run.started"),
                            &run_id,
                            &run.workspace_id,
                            "deployment_run.started",
                        ),
                    )
                    .await?
                {
                    ScheduledRunClaimOutcome::Claimed => {
                        // `tick` may calculate several overdue occurrences at
                        // once. Publish only this transaction's committed
                        // revision before launch so an auto-pause is fenced by
                        // the database revision it actually follows.
                        self.deployments
                            .lock()
                            .expect("Deployment projection lock")
                            .insert(run.deployment_id.clone(), deployment.clone());
                    }
                    ScheduledRunClaimOutcome::AlreadyClaimed
                    | ScheduledRunClaimOutcome::StaleDeployment => {
                        self.runs
                            .lock()
                            .expect("DeploymentRun projection lock")
                            .remove(&run_id);
                        self.refresh().await?;
                        continue;
                    }
                }
            }
            completed.push(self.launch_run(&run_id, launch).await?);
        }
        Ok(completed)
    }

    fn tick(&self, now: u64) -> Result<Vec<ScheduledCandidate>, DeploymentApplicationError> {
        let mut result = Vec::new();
        let mut deployments = self.deployments.lock().expect("Deployment projection lock");
        let mut runs = self.runs.lock().expect("DeploymentRun projection lock");
        for (deployment_id, deployment) in deployments.iter_mut() {
            let Some((cron, timezone)) = active_cron(deployment) else {
                continue;
            };
            let mut cursor = match deployment.next_fire_ms {
                Some(cursor) => cursor,
                None => match cron.next_after_in(now, timezone) {
                    Some(cursor) => cursor,
                    None => continue,
                },
            };
            loop {
                let next = cron.next_after_in(cursor, timezone);
                let interval_ms = next.unwrap_or(cursor).saturating_sub(cursor);
                let due =
                    cursor.saturating_add(execution_jitter_ms(deployment_id, cursor, interval_ms));
                if due > now {
                    break;
                }
                let run_id = format!("drun_{}", uuid::Uuid::new_v4().simple());
                let scheduled_at = timestamp(cursor);
                let expected_revision = deployment.revision;
                let advanced_revision = next_revision(expected_revision)?;
                let run = DeploymentRunRecord {
                    created_at: timestamp(now),
                    deployment_id: deployment_id.clone(),
                    workspace_id: deployment.workspace_id.clone(),
                    agent: deployment.agent.clone(),
                    trigger: DeploymentTrigger::Schedule {
                        scheduled_at: scheduled_at.clone(),
                    },
                    session_id: None,
                    error: None,
                };
                runs.insert(run_id.clone(), run.clone());
                deployment.last_run_at = Some(scheduled_at);
                deployment.next_fire_ms = Some(next.unwrap_or(cursor));
                deployment.revision = advanced_revision;
                result.push((
                    run_id.clone(),
                    launch_for(deployment, deployment_id, &run_id),
                    run,
                    deployment.clone(),
                    expected_revision,
                ));
                cursor = match next {
                    Some(cursor) => cursor,
                    None => break,
                };
                if deployment.revision == MAX_DEPLOYMENT_REVISION {
                    break;
                }
            }
            if deployment.next_fire_ms.is_none() {
                deployment.next_fire_ms = Some(cursor);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::DeploymentLaunchOutcome;
    use crate::tests::{OutcomeLauncher, command};

    #[derive(Default)]
    struct ToggleProjectionRefresh {
        calls: AtomicUsize,
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::ExecutableProjectionRefresh for ToggleProjectionRefresh {
        async fn refresh(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err("projection unavailable".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn jitter_is_stable_and_obeys_all_interval_bounds() {
        // Cause/effect graph: identity + exact scheduled instant select a stable
        // point within the interval-derived bound. Interval <33.34s reaches the
        // 5s floor; ordinary intervals use 15%; intervals >60m reach the 9m cap;
        // extreme timestamps/intervals saturate without wrap.
        //
        // Decision table:
        // | Rule | interval       | bound | effects                         |
        // | J1   | 10s            | 5s    | stable delay in 0..=5s          |
        // | J2   | 15m            | 135s  | stable delay in 0..=15%         |
        // | J3   | 24h            | 9m    | stable delay in 0..=9m          |
        // | J4   | u64::MAX       | 9m    | no arithmetic/due-time wrap     |
        for (rule, interval, bound) in [
            ("J1", 10_000, MIN_JITTER_BOUND_MS),
            ("J2", 15 * 60_000, 135_000),
            ("J3", 24 * 60 * 60_000, MAX_JITTER_BOUND_MS),
            ("J4", u64::MAX, MAX_JITTER_BOUND_MS),
        ] {
            assert_eq!(execution_jitter_bound_ms(interval), bound, "{rule}");
            let delay = execution_jitter_ms("depl-a", u64::MAX, interval);
            assert_eq!(
                delay,
                execution_jitter_ms("depl-a", u64::MAX, interval),
                "{rule}"
            );
            assert!(delay <= bound, "{rule}");
            assert_eq!(u64::MAX.saturating_add(delay), u64::MAX, "{rule}");
        }
    }

    #[tokio::test]
    async fn scheduler_refresh_failure_precedes_candidate_mutation_and_launch() {
        // Causes: C1 a scheduled Deployment is due; C2 executable refresh
        // succeeds/fails; C3 the failed prerequisite later recovers. Effects:
        // E1 failure returns Unavailable before speculative cursor/run mutation
        // or launch; E2 retry claims and launches once. Constraints: K1 the
        // refresh owns no Deployment state; K2 tick remains the only candidate
        // compiler. Decision table: D1 C1+!C2=>E1; D2 C1+C2+C3=>E2.
        let application = DeploymentApplication::new();
        let refresh = Arc::new(ToggleProjectionRefresh::default());
        application.bind_executable_projection_refresh(refresh.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        application.bind_launcher(Arc::new(OutcomeLauncher {
            outcome: DeploymentLaunchOutcome::Created {
                session_id: "session-refresh".into(),
            },
            calls: calls.clone(),
        }));
        let deployment = application.create(command(true)).await.unwrap();
        let scheduled = 1_767_603_600_000;
        {
            let mut deployments = application.deployments.lock().unwrap();
            deployments.get_mut(&deployment.id).unwrap().next_fire_ms = Some(scheduled);
        }
        let due =
            scheduled.saturating_add(execution_jitter_ms(&deployment.id, scheduled, 15 * 60_000));
        let before = application
            .deployments
            .lock()
            .unwrap()
            .get(&deployment.id)
            .unwrap()
            .clone();
        refresh.fail.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                application.tick_and_launch(due).await,
                Err(DeploymentApplicationError::Unavailable(_))
            ),
            "D1/E1"
        );
        assert_eq!(
            application
                .deployments
                .lock()
                .unwrap()
                .get(&deployment.id)
                .unwrap(),
            &before,
            "D1/E1 no cursor mutation"
        );
        assert!(application.runs.lock().unwrap().is_empty(), "D1/E1");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "D1/E1");

        refresh.fail.store(false, Ordering::SeqCst);
        assert_eq!(
            application.tick_and_launch(due).await.unwrap().len(),
            1,
            "D2/E2"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "D2/E2");
    }

    #[tokio::test]
    async fn schedule_cursor_is_future_only_and_pause_suppresses_execution() {
        // Cursor graph: S1 first tick with no cursor -> seed strictly after now;
        // S2 before stable jitter due -> no run; S3 at due -> one run carrying
        // the unjittered cron instant; S4 paused -> no later runs; S5 unpause ->
        // reseed after unpause time and never backfill missed occurrences.
        const MONDAY_0900: u64 = 1_767_603_600_000;
        let application = DeploymentApplication::new();
        let mut create = command(true);
        create.schedule = Some(DeploymentSchedule::Cron {
            expression: "*/15 * * * *".into(),
            timezone: "UTC".into(),
        });
        let deployment = application.create(create).await.unwrap();
        {
            let mut records = application.deployments.lock().unwrap();
            records.get_mut(&deployment.id).unwrap().next_fire_ms = None;
        }
        assert!(application.tick(MONDAY_0900).unwrap().is_empty(), "S1");
        let scheduled = MONDAY_0900 + 15 * 60_000;
        let due =
            scheduled.saturating_add(execution_jitter_ms(&deployment.id, scheduled, 15 * 60_000));
        assert!(application.tick(due - 1).unwrap().is_empty(), "S2");
        let fired = application.tick(due).unwrap();
        assert_eq!(fired.len(), 1, "S3");
        assert!(
            matches!(
                &fired[0].2.trigger,
                DeploymentTrigger::Schedule { scheduled_at }
                    if scheduled_at == "2026-01-05T09:15:00Z"
            ),
            "S3"
        );
        application
            .pause("workspace-a", &deployment.id)
            .await
            .unwrap();
        assert!(
            application
                .tick(MONDAY_0900 + 2 * 60 * 60_000)
                .unwrap()
                .is_empty(),
            "S4"
        );
        application
            .unpause("workspace-a", &deployment.id)
            .await
            .unwrap();
        assert!(
            application
                .get("workspace-a", &deployment.id)
                .await
                .unwrap()
                .record
                .next_fire_ms
                .is_some_and(|cursor| cursor > crate::now_ms()),
            "S5"
        );
    }
}
