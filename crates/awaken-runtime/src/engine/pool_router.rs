//! Pure routing logic for a model pool.
//!
//! [`PoolRouter`] owns the member metadata and the routing/switch policies and
//! answers three questions, all as pure functions of its inputs:
//!
//! 1. **Home selection** — which member a session starts on, chosen by a stable
//!    weighted-rendezvous hash of the routing key so the same agent always
//!    homes to the same member (prompt-cache affinity) while different agents
//!    spread across the pool.
//! 2. **Failover selection** — given the active member and a health mask, which
//!    member to move to.
//! 3. **Switch decision** — whether a given inference error warrants leaving the
//!    current member at all (quota / permanent), as opposed to a transient blip
//!    that the member's own retry policy should absorb.
//!
//! Health is supplied by the caller as a mask aligned with the member list, so
//! the router stays free of circuit-breaker timing and is trivially testable.
//! The executor (which owns the [`CircuitBreaker`](super::circuit_breaker)) and
//! the sticky per-session state build on top of these decisions.

use awaken_contract::contract::executor::InferenceExecutionError;
use awaken_contract::registry_spec::{
    HomeStrategy, PoolMemberRole, PoolRoutingPolicy, PoolSwitchPolicy, StickyScope,
};

/// A resolved pool member as the router sees it: addressing plus selection
/// metadata. The executor pairs each with its concrete `LlmExecutor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterMember {
    /// The member's `ModelSpec.id`, used as the circuit-breaker key.
    pub model_id: String,
    /// Home eligibility.
    pub role: PoolMemberRole,
    /// Selection weight (already defaulted from `Option<u32>` to at least 1).
    pub weight: u32,
}

/// Health of the members, aligned by index with [`PoolRouter`]'s member list.
pub type HealthMask = [bool];

/// Pure routing decisions over a fixed set of members.
#[derive(Debug, Clone)]
pub struct PoolRouter {
    members: Vec<RouterMember>,
    routing: PoolRoutingPolicy,
    switch: PoolSwitchPolicy,
}

impl PoolRouter {
    /// Build a router over the given members and policies. `members` must be
    /// non-empty with at least one `Member`-role entry (guaranteed by
    /// `validate_model_pool_spec`).
    pub fn new(
        members: Vec<RouterMember>,
        routing: PoolRoutingPolicy,
        switch: PoolSwitchPolicy,
    ) -> Self {
        Self {
            members,
            routing,
            switch,
        }
    }

    /// The members the router routes over, in declaration order.
    pub fn members(&self) -> &[RouterMember] {
        &self.members
    }

    /// The switch policy governing failover triggers.
    pub fn switch_policy(&self) -> &PoolSwitchPolicy {
        &self.switch
    }

    pub fn sticky_scope(&self) -> StickyScope {
        self.routing.sticky_scope
    }

    pub fn home_strategy(&self) -> HomeStrategy {
        self.routing.home
    }

    /// Choose the home member for `routing_key`.
    ///
    /// Among `Member`-role entries, the highest weighted-rendezvous score for
    /// the key wins, restricted to healthy members when at least one is
    /// healthy. If every home-eligible member is unhealthy the best-scoring
    /// home-eligible member is returned anyway (the executor will still attempt
    /// it rather than route to a `FailoverOnly` member as the cache-affinity
    /// home).
    pub fn select_home(&self, routing_key: &str, healthy: &HealthMask) -> usize {
        self.select_home_with_sequence(routing_key, healthy, None)
    }

    pub fn select_home_with_sequence(
        &self,
        routing_key: &str,
        healthy: &HealthMask,
        sequence: Option<u64>,
    ) -> usize {
        if matches!(self.routing.home, HomeStrategy::RoundRobin) {
            return self
                .round_robin_home(healthy, sequence.unwrap_or(0))
                .unwrap_or(0);
        }
        let eligible = |idx: usize| self.members[idx].role == PoolMemberRole::Member;
        // Prefer healthy home-eligible members; fall back to any home-eligible.
        self.best_by_score(routing_key, |idx| {
            eligible(idx) && Self::is_healthy(healthy, idx)
        })
        .or_else(|| self.best_by_score(routing_key, eligible))
        .unwrap_or(0)
    }

    fn round_robin_home(&self, healthy: &HealthMask, sequence: u64) -> Option<usize> {
        let choose = |require_healthy: bool| {
            let total_weight: u64 = self
                .members
                .iter()
                .enumerate()
                .filter(|(idx, member)| {
                    member.role == PoolMemberRole::Member
                        && (!require_healthy || Self::is_healthy(healthy, *idx))
                })
                .map(|(_, member)| u64::from(member.weight.max(1)))
                .sum();
            if total_weight == 0 {
                return None;
            }
            let mut slot = sequence % total_weight;
            for (idx, member) in self.members.iter().enumerate() {
                if member.role != PoolMemberRole::Member
                    || (require_healthy && !Self::is_healthy(healthy, idx))
                {
                    continue;
                }
                let weight = u64::from(member.weight.max(1));
                if slot < weight {
                    return Some(idx);
                }
                slot -= weight;
            }
            None
        };
        choose(true).or_else(|| choose(false))
    }

    /// Choose a failover target distinct from `current_idx`, among healthy
    /// members of any role. Returns `None` when no healthy alternative exists.
    pub fn select_failover(
        &self,
        routing_key: &str,
        current_idx: usize,
        healthy: &HealthMask,
    ) -> Option<usize> {
        self.best_by_score(routing_key, |idx| {
            idx != current_idx && Self::is_healthy(healthy, idx)
        })
    }

    /// Whether `err` warrants leaving the current member.
    ///
    /// Quota signals (rate-limit / overload) switch when [`PoolSwitchPolicy::on_quota`]
    /// is set and any retry-after threshold is met; permanent member errors
    /// (unauthorized / model-not-found) switch when [`PoolSwitchPolicy::on_permanent`]
    /// is set. Transient and request-level errors never switch — the former are
    /// absorbed by the member's own retry policy, the latter would fail
    /// identically on every member.
    pub fn should_switch_on_error(&self, err: &InferenceExecutionError) -> bool {
        match err {
            InferenceExecutionError::RateLimited { retry_after, .. }
            | InferenceExecutionError::Overloaded { retry_after, .. } => {
                self.switch.on_quota && self.quota_threshold_met(*retry_after)
            }
            InferenceExecutionError::Unauthorized(_)
            | InferenceExecutionError::ModelNotFound(_) => self.switch.on_permanent,
            InferenceExecutionError::StreamInterrupted { .. } => self.switch.on_circuit_open,
            _ => false,
        }
    }

    /// A quota signal is switch-worthy unless a threshold is configured and the
    /// provider's retry-after is below it (worth waiting out in place). An
    /// absent retry-after is treated as switch-worthy rather than blocking for
    /// an unknown duration.
    fn quota_threshold_met(&self, retry_after: Option<std::time::Duration>) -> bool {
        match (self.switch.quota_retry_after_threshold_secs, retry_after) {
            (Some(threshold), Some(after)) => after.as_secs() >= threshold,
            _ => true,
        }
    }

    fn is_healthy(healthy: &HealthMask, idx: usize) -> bool {
        healthy.get(idx).copied().unwrap_or(true)
    }

    /// Index of the candidate (per `accept`) with the highest weighted
    /// rendezvous score for `routing_key`, ties broken by lowest index.
    fn best_by_score(&self, routing_key: &str, accept: impl Fn(usize) -> bool) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (idx, member) in self.members.iter().enumerate() {
            if !accept(idx) {
                continue;
            }
            let score = match self.routing.home {
                HomeStrategy::Deterministic | HomeStrategy::RoundRobin => {
                    weighted_rendezvous_score(routing_key, &member.model_id, member.weight)
                }
                // First healthy in declaration order: decreasing score by index.
                HomeStrategy::FirstHealthy => -(idx as f64),
            };
            match best {
                Some((_, best_score)) if score <= best_score => {}
                _ => best = Some((idx, score)),
            }
        }
        best.map(|(idx, _)| idx)
    }
}

/// Weighted-rendezvous (HRW) score: a stable hash of `(routing_key, model_id)`
/// shaped by `weight` so a member's chance of winning scales with its weight.
/// Deterministic across processes for a fixed input.
fn weighted_rendezvous_score(routing_key: &str, model_id: &str, weight: u32) -> f64 {
    let h = fnv1a(routing_key, model_id);
    // Map to (0, 1): +1 / +2 avoid exactly 0 or 1.
    let unit = (h as f64 + 1.0) / (u64::MAX as f64 + 2.0);
    f64::from(weight.max(1)) / -unit.ln()
}

/// FNV-1a over `routing_key`, a separator, and `model_id`. Stable and
/// dependency-free so home assignment is reproducible.
fn fnv1a(routing_key: &str, model_id: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in routing_key
        .as_bytes()
        .iter()
        .chain(std::slice::from_ref(&0u8))
        .chain(model_id.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn member(id: &str, role: PoolMemberRole, weight: u32) -> RouterMember {
        RouterMember {
            model_id: id.to_string(),
            role,
            weight,
        }
    }

    fn router(members: Vec<RouterMember>) -> PoolRouter {
        PoolRouter::new(
            members,
            PoolRoutingPolicy::default(),
            PoolSwitchPolicy::default(),
        )
    }

    fn router_with_switch(members: Vec<RouterMember>, switch: PoolSwitchPolicy) -> PoolRouter {
        PoolRouter::new(members, PoolRoutingPolicy::default(), switch)
    }

    fn router_with_routing(members: Vec<RouterMember>, routing: PoolRoutingPolicy) -> PoolRouter {
        PoolRouter::new(members, routing, PoolSwitchPolicy::default())
    }

    // ── switch decision ──

    #[test]
    fn switches_on_quota_and_permanent_by_default() {
        let r = router(vec![member("a", PoolMemberRole::Member, 1)]);
        assert!(r.should_switch_on_error(&InferenceExecutionError::rate_limited("429")));
        assert!(r.should_switch_on_error(&InferenceExecutionError::overloaded("529")));
        assert!(r.should_switch_on_error(&InferenceExecutionError::Unauthorized("401".into())));
        assert!(r.should_switch_on_error(&InferenceExecutionError::ModelNotFound("404".into())));
    }

    #[test]
    fn does_not_switch_on_transient_or_request_errors() {
        let r = router(vec![member("a", PoolMemberRole::Member, 1)]);
        // Transient — the member's own retry policy absorbs these.
        assert!(!r.should_switch_on_error(&InferenceExecutionError::Provider("blip".into())));
        assert!(!r.should_switch_on_error(&InferenceExecutionError::Timeout("slow".into())));
        // Request-level — would fail identically on any member.
        assert!(!r.should_switch_on_error(&InferenceExecutionError::ContextOverflow("big".into())));
        assert!(!r.should_switch_on_error(&InferenceExecutionError::InvalidRequest("bad".into())));
        assert!(!r.should_switch_on_error(&InferenceExecutionError::Cancelled));
        assert!(!r.should_switch_on_error(&InferenceExecutionError::AllModelsUnavailable));
    }

    #[test]
    fn respects_disabled_switch_triggers() {
        let switch = PoolSwitchPolicy {
            on_quota: false,
            on_permanent: false,
            ..PoolSwitchPolicy::default()
        };
        let r = router_with_switch(vec![member("a", PoolMemberRole::Member, 1)], switch);
        assert!(!r.should_switch_on_error(&InferenceExecutionError::rate_limited("429")));
        assert!(!r.should_switch_on_error(&InferenceExecutionError::Unauthorized("401".into())));
    }

    #[test]
    fn quota_threshold_gates_on_retry_after() {
        let switch = PoolSwitchPolicy {
            quota_retry_after_threshold_secs: Some(60),
            ..PoolSwitchPolicy::default()
        };
        let r = router_with_switch(vec![member("a", PoolMemberRole::Member, 1)], switch);
        // Short cool-off: wait it out on the same member.
        assert!(
            !r.should_switch_on_error(&InferenceExecutionError::Overloaded {
                message: "529".into(),
                retry_after: Some(Duration::from_secs(30)),
            })
        );
        // Long cool-off: switch.
        assert!(
            r.should_switch_on_error(&InferenceExecutionError::Overloaded {
                message: "529".into(),
                retry_after: Some(Duration::from_secs(120)),
            })
        );
        // Unknown cool-off: switch rather than block indefinitely.
        assert!(r.should_switch_on_error(&InferenceExecutionError::rate_limited("429")));
    }

    // ── home selection ──

    #[test]
    fn home_is_deterministic_for_a_key() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::Member, 1),
            member("c", PoolMemberRole::Member, 1),
        ]);
        let healthy = [true, true, true];
        let first = r.select_home("agent-x", &healthy);
        for _ in 0..10 {
            assert_eq!(r.select_home("agent-x", &healthy), first);
        }
    }

    #[test]
    fn home_distributes_across_members() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::Member, 1),
        ]);
        let healthy = [true, true];
        let mut seen = std::collections::HashSet::new();
        for i in 0..200 {
            seen.insert(r.select_home(&format!("agent-{i}"), &healthy));
        }
        assert_eq!(seen.len(), 2, "both members should receive some agents");
    }

    #[test]
    fn round_robin_home_advances_by_session_sequence() {
        let r = router_with_routing(
            vec![
                member("a", PoolMemberRole::Member, 1),
                member("b", PoolMemberRole::Member, 1),
                member("c", PoolMemberRole::Member, 1),
            ],
            PoolRoutingPolicy {
                home: HomeStrategy::RoundRobin,
                ..PoolRoutingPolicy::default()
            },
        );
        let healthy = [true, true, true];
        let homes: Vec<_> = (0..6)
            .map(|i| r.select_home_with_sequence("same-key", &healthy, Some(i)))
            .collect();
        assert_eq!(homes, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn round_robin_home_skips_unhealthy_and_respects_weights() {
        let r = router_with_routing(
            vec![
                member("a", PoolMemberRole::Member, 2),
                member("b", PoolMemberRole::Member, 1),
                member("c", PoolMemberRole::Member, 1),
            ],
            PoolRoutingPolicy {
                home: HomeStrategy::RoundRobin,
                ..PoolRoutingPolicy::default()
            },
        );
        let healthy = [true, false, true];
        let homes: Vec<_> = (0..6)
            .map(|i| r.select_home_with_sequence("same-key", &healthy, Some(i)))
            .collect();
        assert_eq!(homes, vec![0, 0, 2, 0, 0, 2]);
    }

    #[test]
    fn home_never_selects_failover_only() {
        let r = router(vec![
            member("a", PoolMemberRole::FailoverOnly, 1),
            member("b", PoolMemberRole::Member, 1),
        ]);
        let healthy = [true, true];
        for i in 0..50 {
            assert_eq!(r.select_home(&format!("k-{i}"), &healthy), 1);
        }
    }

    #[test]
    fn home_avoids_unhealthy_when_healthy_alternative_exists() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::Member, 1),
        ]);
        let healthy = [false, true];
        for i in 0..50 {
            assert_eq!(r.select_home(&format!("k-{i}"), &healthy), 1);
        }
    }

    #[test]
    fn home_falls_back_to_unhealthy_home_when_none_healthy() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::FailoverOnly, 1),
        ]);
        // No healthy member: must still return the home-eligible member, not the
        // failover-only one.
        let healthy = [false, false];
        assert_eq!(r.select_home("k", &healthy), 0);
    }

    // ── failover selection ──

    #[test]
    fn failover_returns_a_healthy_other_member() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::Member, 1),
            member("c", PoolMemberRole::FailoverOnly, 1),
        ]);
        let healthy = [true, true, true];
        let next = r.select_failover("agent-x", 0, &healthy).expect("a target");
        assert_ne!(next, 0);
    }

    #[test]
    fn failover_skips_unhealthy_members() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::Member, 1),
            member("c", PoolMemberRole::FailoverOnly, 1),
        ]);
        // Only the failover-only member is healthy.
        let healthy = [false, false, true];
        assert_eq!(r.select_failover("agent-x", 0, &healthy), Some(2));
    }

    #[test]
    fn failover_returns_none_when_no_healthy_alternative() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::Member, 1),
        ]);
        let healthy = [true, false];
        assert_eq!(r.select_failover("agent-x", 0, &healthy), None);
    }

    #[test]
    fn failover_is_deterministic_for_a_key() {
        let r = router(vec![
            member("a", PoolMemberRole::Member, 1),
            member("b", PoolMemberRole::Member, 1),
            member("c", PoolMemberRole::Member, 1),
        ]);
        let healthy = [true, true, true];
        let first = r.select_failover("agent-x", 0, &healthy);
        for _ in 0..10 {
            assert_eq!(r.select_failover("agent-x", 0, &healthy), first);
        }
    }
}
