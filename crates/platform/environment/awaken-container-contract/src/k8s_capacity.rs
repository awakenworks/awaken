//! Kubernetes' two "no room" answers and the bounded wait for the scheduler
//! (ADR-0092). Pure policy shared by the Kubernetes adapter's direct and
//! brokered create transports and by its readiness wait; no I/O.

use std::time::Duration;

/// Default bound on waiting for the scheduler, long enough for a cluster
/// autoscaler to add a node. Operators set `k8s_unschedulable_wait_secs`.
pub const DEFAULT_K8S_UNSCHEDULABLE_WAIT: Duration = Duration::from_secs(300);

/// Kubernetes' `ResourceQuota` admission refuses an object that would exceed
/// the namespace quota with 403 and a `Status` message carrying this marker.
const QUOTA_EXCEEDED_MARKER: &str = "exceeded quota";

/// Whether an API refusal means "no room now": only a 403 whose `Status`
/// message carries the quota admission's `exceeded quota` marker. Every other
/// 403 (RBAC, the Coordinator broker's own `Forbidden`, a quota that demands
/// explicit requests -- `failed quota: ... must specify` -- or a `LimitRange`)
/// stays an authority refusal.
#[must_use]
pub fn kubernetes_refusal_is_capacity(status: u16, message: &str) -> bool {
    status == 403 && message.contains(QUOTA_EXCEEDED_MARKER)
}

/// One Pod readiness observation, judged from its phase and container states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum K8sPodReadiness {
    Ready,
    Waiting(String),
    Failed(String),
}

/// One `PodScheduled` condition as the API reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct K8sPodScheduledCondition<'a> {
    pub status: &'a str,
    pub reason: Option<&'a str>,
    pub message: Option<&'a str>,
}

/// The scheduler's refusal to place a Pending Pod (`PodScheduled=False`,
/// reason `Unschedulable`), carrying the scheduler's message. A Pod the
/// scheduler has not looked at yet, a scheduled Pod, another reason (for
/// example `SchedulingGated`) or a non-Pending phase is not a refusal.
#[must_use]
pub fn k8s_pod_unschedulable(
    phase: Option<&str>,
    scheduled: Option<K8sPodScheduledCondition<'_>>,
) -> Option<String> {
    let condition = scheduled.filter(|condition| {
        phase == Some("Pending")
            && condition.status == "False"
            && condition.reason == Some("Unschedulable")
    })?;
    Some(
        condition
            .message
            .filter(|message| !message.trim().is_empty())
            .unwrap_or("the scheduler cannot place the Pod")
            .to_owned(),
    )
}

/// One readiness observation's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum K8sPodWait {
    Ready,
    /// Observe again after the poll interval.
    Poll,
    /// A container-level fast failure: the Pod can never become ready.
    CannotStart(String),
    /// A scheduled Pod did not finish starting within the start-up timeout.
    StartupTimedOut(String),
    /// The scheduler still cannot place the Pod after the configured bound.
    Unschedulable(String),
}

/// Decide one observation. `since_creation` is measured from Pod creation and
/// bounds only the wait for the scheduler; `startup_elapsed` is measured from
/// the last observation in which the Pod was unschedulable (or the start of
/// the wait) and bounds only start-up, so neither timeout measures the other.
#[must_use]
pub fn k8s_pod_wait(
    readiness: K8sPodReadiness,
    unschedulable: Option<String>,
    since_creation: Duration,
    unschedulable_bound: Duration,
    startup_elapsed: Duration,
    startup_timeout: Duration,
) -> K8sPodWait {
    match (readiness, unschedulable) {
        (K8sPodReadiness::Ready, _) => K8sPodWait::Ready,
        (K8sPodReadiness::Failed(reason), _) => K8sPodWait::CannotStart(reason),
        (K8sPodReadiness::Waiting(_), Some(message)) if since_creation >= unschedulable_bound => {
            K8sPodWait::Unschedulable(message)
        }
        (K8sPodReadiness::Waiting(_), Some(_)) => K8sPodWait::Poll,
        (K8sPodReadiness::Waiting(reason), None) if startup_elapsed >= startup_timeout => {
            K8sPodWait::StartupTimedOut(reason)
        }
        (K8sPodReadiness::Waiting(_), None) => K8sPodWait::Poll,
    }
}

/// The provisioning deadline of a Sandbox Pod created at `created_ms`: the
/// last instant the creating attempt's own readiness wait ([`k8s_pod_wait`])
/// could still be waiting. The scheduler bound runs from creation and the
/// start-up timeout from the last unschedulable observation, so the latest
/// give-up is creation + scheduler bound + start-up timeout. A recovering
/// observer therefore never waits longer than the creator would have.
#[must_use]
pub fn k8s_pod_provisioning_deadline_ms(
    created_ms: u64,
    unschedulable_bound: Duration,
    startup_timeout: Duration,
) -> u64 {
    let bound = unschedulable_bound
        .saturating_add(startup_timeout)
        .as_millis();
    created_ms.saturating_add(u64::try_from(bound).unwrap_or(u64::MAX))
}

/// Time since `created_ms` at `now_ms`; a reading before creation (clock
/// skew) is zero.
#[must_use]
pub fn k8s_pod_age(created_ms: u64, now_ms: u64) -> Duration {
    Duration::from_millis(now_ms.saturating_sub(created_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pod_provisioning_deadline_is_the_creators_latest_give_up() {
        // Decision table: C1 creation instant; C2 scheduler bound; C3 start-up
        // timeout. Effect: creation + C2 + C3, saturating at the end of time.
        let secs = Duration::from_secs;
        for (rule, created, bound, startup, deadline) in [
            ("P1 defaults", 1_000, secs(300), secs(120), 421_000),
            ("P2 no scheduler wait", 1_000, secs(0), secs(120), 121_000),
            ("P3 saturates", u64::MAX - 5, secs(300), secs(120), u64::MAX),
            ("P3 saturates", 0, Duration::MAX, secs(120), u64::MAX),
        ] {
            assert_eq!(
                k8s_pod_provisioning_deadline_ms(created, bound, startup),
                deadline,
                "{rule}"
            );
        }
    }

    #[test]
    fn only_a_quota_exhaustion_refusal_is_capacity() {
        // Decision table (ADR-0092 decision 1). Causes: C1 HTTP status; C2 the
        // message carries the quota admission's marker.
        // | Rule | C1 | C2 | capacity |
        // | Q1 | 403 | yes | yes |
        // | Q2 | 403 | no (RBAC, broker Forbidden, failed quota, LimitRange) | no |
        // | Q3 | 401/404/409/422/500 | yes | no |
        let quota = "pods \"s\" is forbidden: exceeded quota: compute, requested: pods=1";
        assert!(kubernetes_refusal_is_capacity(403, quota), "Q1");
        for message in [
            "pods is forbidden: User \"w\" cannot create resource \"pods\"",
            "Forbidden",
            "pods \"s\" is forbidden: failed quota: compute: must specify limits.cpu",
            "pods \"s\" is forbidden: maximum cpu usage per Container is 2, but limit is 4",
        ] {
            assert!(
                !kubernetes_refusal_is_capacity(403, message),
                "Q2 {message}"
            );
        }
        for status in [401, 404, 409, 422, 500] {
            assert!(
                !kubernetes_refusal_is_capacity(status, quota),
                "Q3 {status}"
            );
        }
    }

    #[test]
    fn only_a_pending_pod_the_scheduler_refused_is_unschedulable() {
        // U1 Pending + PodScheduled=False/Unschedulable -> the scheduler's
        // message (a default when empty); U2 no condition yet, U3 scheduled,
        // U4 another reason, U5 a non-Pending phase -> not a refusal.
        let condition = |status, reason, message| {
            Some(K8sPodScheduledCondition {
                status,
                reason,
                message,
            })
        };
        let refused = condition(
            "False",
            Some("Unschedulable"),
            Some("0/3 nodes: Insufficient cpu"),
        );
        assert_eq!(
            k8s_pod_unschedulable(Some("Pending"), refused),
            Some("0/3 nodes: Insufficient cpu".into()),
            "U1"
        );
        assert_eq!(
            k8s_pod_unschedulable(
                Some("Pending"),
                condition("False", Some("Unschedulable"), None)
            ),
            Some("the scheduler cannot place the Pod".into()),
            "U1 default"
        );
        assert_eq!(k8s_pod_unschedulable(Some("Pending"), None), None, "U2");
        assert_eq!(
            k8s_pod_unschedulable(Some("Pending"), condition("True", None, None)),
            None,
            "U3"
        );
        assert_eq!(
            k8s_pod_unschedulable(
                Some("Pending"),
                condition("False", Some("SchedulingGated"), None)
            ),
            None,
            "U4"
        );
        assert_eq!(k8s_pod_unschedulable(Some("Running"), refused), None, "U5");
    }

    #[test]
    fn schedulability_wait_is_bounded_from_creation_and_startup_from_scheduling() {
        // Decision table (ADR-0092 decision 2). Causes: C1 readiness; C2 the
        // scheduler refuses the Pod; C3 time since creation vs the bound; C4
        // start-up time since the last refusal vs the start-up timeout.
        // | Rule | C1 | C2 | C3 | C4 | effect |
        // | S1 | Ready | any | any | any | Ready |
        // | S2 | Failed | any | any | any | CannotStart (fast failures unchanged) |
        // | S3 | Waiting | yes | below | any (even past) | Poll: never the start-up timeout |
        // | S4 | Waiting | yes | at/past | any | Unschedulable |
        // | S5 | Waiting | no | any (even past) | below | Poll: never the capacity bound |
        // | S6 | Waiting | no | any | at/past | StartupTimedOut |
        // | S7 | Waiting | yes | bound 0 | any | the first observation is Unschedulable |
        let bound = Duration::from_secs(300);
        let startup = Duration::from_secs(120);
        let s = Duration::from_secs;
        let waiting = || K8sPodReadiness::Waiting("Pod phase is Pending".into());
        let refused = || Some("0/3 nodes are available".to_string());
        let rows = [
            (
                "S1",
                K8sPodReadiness::Ready,
                refused(),
                s(999),
                bound,
                s(999),
                K8sPodWait::Ready,
            ),
            (
                "S2",
                K8sPodReadiness::Failed("agent container ErrImagePull".into()),
                None,
                s(0),
                bound,
                s(0),
                K8sPodWait::CannotStart("agent container ErrImagePull".into()),
            ),
            (
                "S3",
                waiting(),
                refused(),
                s(299),
                bound,
                s(500),
                K8sPodWait::Poll,
            ),
            (
                "S4",
                waiting(),
                refused(),
                s(300),
                bound,
                s(0),
                K8sPodWait::Unschedulable("0/3 nodes are available".into()),
            ),
            (
                "S5",
                waiting(),
                None,
                s(10_000),
                bound,
                s(119),
                K8sPodWait::Poll,
            ),
            (
                "S6",
                waiting(),
                None,
                s(0),
                bound,
                s(120),
                K8sPodWait::StartupTimedOut("Pod phase is Pending".into()),
            ),
            (
                "S7",
                waiting(),
                refused(),
                s(0),
                Duration::ZERO,
                s(0),
                K8sPodWait::Unschedulable("0/3 nodes are available".into()),
            ),
        ];
        for (rule, readiness, unschedulable, since_creation, bound, startup_elapsed, expected) in
            rows
        {
            assert_eq!(
                k8s_pod_wait(
                    readiness,
                    unschedulable,
                    since_creation,
                    bound,
                    startup_elapsed,
                    startup
                ),
                expected,
                "{rule}"
            );
        }
        // Clock skew never makes a Pod older than zero.
        assert_eq!(k8s_pod_age(10_000, 52_000), s(42));
        assert_eq!(k8s_pod_age(10_000, 5_000), Duration::ZERO);
    }
}
