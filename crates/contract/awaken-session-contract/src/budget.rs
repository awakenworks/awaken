//! Exact Managed list-cost budget values owned by the Session aggregate.

use std::collections::BTreeMap;

use async_trait::async_trait;

/// USD list-price rate in micro-dollars per one million tokens.
///
/// Multiplying this value by a token count yields a numerator whose fixed
/// denominator is one million. Keeping that remainder in the aggregate avoids
/// per-request rounding and makes replay deterministic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedTokenListRates {
    pub input_micros_per_million: u64,
    pub output_micros_per_million: u64,
    pub cache_read_micros_per_million: u64,
    pub cache_creation_micros_per_million: u64,
}

/// Non-token public list rates carried in the same immutable snapshot.
///
/// Active time uses a per-million-seconds representation for the same reason
/// token prices use per-million-token values: the Session can accumulate an
/// exact integer numerator and postpone rounding until wire projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedRuntimeListRates {
    pub active_micros_per_million_seconds: u64,
    pub web_fetch_micros_per_request: u64,
    pub web_search_micros_per_request: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedListPriceSnapshot {
    pub snapshot_id: String,
    pub version: u32,
    pub effective_at_unix_ms: u64,
    pub arithmetic_version: u32,
    pub model_rates: BTreeMap<String, ManagedTokenListRates>,
    pub runtime_rates: ManagedRuntimeListRates,
    pub fingerprint: String,
}

impl ManagedListPriceSnapshot {
    pub fn validate(&self, required_models: &[String]) -> Result<(), ManagedListPriceError> {
        if self.snapshot_id.trim().is_empty()
            || self.version == 0
            || self.arithmetic_version == 0
            || self.fingerprint.trim().is_empty()
        {
            return Err(ManagedListPriceError::InvalidSnapshot(
                "snapshot identity, version, arithmetic version and fingerprint are required"
                    .into(),
            ));
        }
        for model in required_models {
            if !self.model_rates.contains_key(model) {
                return Err(ManagedListPriceError::MissingModel(model.clone()));
            }
        }
        Ok(())
    }

    pub fn usage_cost_numerator(
        &self,
        usage: ManagedBudgetUsageCursor,
    ) -> Result<u128, ManagedListPriceError> {
        let mut cost = 0_u128;
        for (model, model_usage) in usage.by_model {
            let rates = self
                .model_rates
                .get(&model)
                .ok_or_else(|| ManagedListPriceError::MissingModel(model.clone()))?;
            cost = cost
                .checked_add(
                    checked_model_cost(model_usage, *rates).ok_or_else(list_cost_overflow)?,
                )
                .ok_or_else(|| {
                    ManagedListPriceError::InvalidSnapshot("list cost overflow".into())
                })?;
        }
        cost = cost
            .checked_add(
                checked_cost_term(
                    usage.active_seconds,
                    self.runtime_rates.active_micros_per_million_seconds,
                    1,
                )
                .ok_or_else(list_cost_overflow)?,
            )
            .and_then(|value| {
                checked_cost_term(
                    usage.web_fetch_requests,
                    self.runtime_rates.web_fetch_micros_per_request,
                    SessionBudgetState::COST_DENOMINATOR,
                )
                .and_then(|term| value.checked_add(term))
            })
            .and_then(|value| {
                checked_cost_term(
                    usage.web_search_requests,
                    self.runtime_rates.web_search_micros_per_request,
                    SessionBudgetState::COST_DENOMINATOR,
                )
                .and_then(|term| value.checked_add(term))
            })
            .ok_or_else(list_cost_overflow)?;
        Ok(cost)
    }

    /// Price one logical Thread with the same immutable snapshot and exact
    /// arithmetic used by the Session budget owner, then apply the public
    /// monetary-unit rounding independently for that Thread.
    pub fn public_usage_list_cost_minor(
        &self,
        usage: ManagedBudgetUsageCursor,
    ) -> Result<u64, ManagedListPriceError> {
        public_list_cost_minor(self.usage_cost_numerator(usage)?)
    }
}

fn checked_cost_term(quantity: u64, rate: u64, scale: u128) -> Option<u128> {
    match u128::from(quantity).checked_mul(u128::from(rate)) {
        Some(value) => value.checked_mul(scale),
        None => None,
    }
}

fn checked_model_cost(
    usage: ManagedModelUsageCursor,
    rates: ManagedTokenListRates,
) -> Option<u128> {
    let input = checked_cost_term(usage.input_tokens, rates.input_micros_per_million, 1)?;
    let output = checked_cost_term(usage.output_tokens, rates.output_micros_per_million, 1)?;
    let cache_read = checked_cost_term(
        usage.cache_read_tokens,
        rates.cache_read_micros_per_million,
        1,
    )?;
    let cache_creation = checked_cost_term(
        usage.cache_creation_tokens,
        rates.cache_creation_micros_per_million,
        1,
    )?;
    checked_sum4([input, output, cache_read, cache_creation])
}

fn checked_sum4(terms: [u128; 4]) -> Option<u128> {
    terms
        .into_iter()
        .try_fold(0_u128, |total, term| total.checked_add(term))
}

fn list_cost_overflow() -> ManagedListPriceError {
    ManagedListPriceError::InvalidSnapshot("list cost overflow".into())
}

#[cfg(kani)]
#[kani::proof]
fn accepted_managed_budget_cost_never_wraps() {
    let terms = [kani::any(), kani::any(), kani::any(), kani::any()];
    if let Some(total) = checked_sum4(terms) {
        for term in terms {
            assert!(total >= term);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedListPriceRequest {
    pub occurred_at_unix_ms: u64,
    pub model_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManagedListPriceError {
    #[error("Managed list price is missing for model `{0}`")]
    MissingModel(String),
    #[error("Managed list-price snapshot is invalid: {0}")]
    InvalidSnapshot(String),
    #[error("Managed list-price authority is unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait ManagedListPriceProvider: Send + Sync {
    async fn resolve_snapshot(
        &self,
        request: ManagedListPriceRequest,
    ) -> Result<ManagedListPriceSnapshot, ManagedListPriceError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedBudgetUsageCursor {
    pub by_model: BTreeMap<String, ManagedModelUsageCursor>,
    pub active_seconds: u64,
    pub web_fetch_requests: u64,
    pub web_search_requests: u64,
}

impl ManagedBudgetUsageCursor {
    /// Whether `self` is a monotonic successor of `previous` in every cumulative
    /// counter. Session history and budget accounting share this one check so the
    /// two root-CAS projections cannot accept different rewinds.
    #[must_use]
    pub fn is_at_least(&self, previous: &Self) -> bool {
        self.active_seconds >= previous.active_seconds
            && self.web_fetch_requests >= previous.web_fetch_requests
            && self.web_search_requests >= previous.web_search_requests
            && previous.by_model.iter().all(|(model, prior)| {
                let current = self.by_model.get(model).copied().unwrap_or_default();
                current.input_tokens >= prior.input_tokens
                    && current.output_tokens >= prior.output_tokens
                    && current.cache_read_tokens >= prior.cache_read_tokens
                    && current.cache_creation_tokens >= prior.cache_creation_tokens
            })
    }

    /// Convert the neutral cumulative Session/Thread usage vocabulary once at
    /// the pricing boundary. A legacy tally without per-model attribution may
    /// use the already-frozen model supplied by its Session baseline; callers
    /// must never resolve a mutable current model here.
    pub fn from_session_usage(
        usage: &crate::SessionUsage,
        frozen_fallback_model: Option<&str>,
    ) -> Result<Self, ManagedListPriceError> {
        let mut by_model = usage
            .by_model
            .iter()
            .map(|(model, usage)| {
                (
                    model.clone(),
                    ManagedModelUsageCursor {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        if by_model.is_empty()
            && (usage.input_tokens != 0
                || usage.output_tokens != 0
                || usage.cache_read_tokens != 0
                || usage.cache_creation_tokens != 0)
        {
            let model = frozen_fallback_model
                .filter(|model| !model.trim().is_empty())
                .ok_or_else(|| {
                    ManagedListPriceError::InvalidSnapshot(
                        "token usage has no served-model attribution".into(),
                    )
                })?;
            by_model.insert(
                model.to_owned(),
                ManagedModelUsageCursor {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_creation_tokens: usage.cache_creation_tokens,
                },
            );
        }
        Ok(Self {
            by_model,
            active_seconds: usage.active_seconds,
            web_fetch_requests: usage.web_fetch_requests,
            web_search_requests: usage.web_search_requests,
        })
    }

    /// Reconstruct the neutral cumulative usage represented by this exact
    /// pricing cursor. This inverse projection is used for durable cap events;
    /// it does not consult current Runtime counters.
    pub fn to_session_usage(&self) -> Result<crate::SessionUsage, ManagedListPriceError> {
        let mut usage = crate::SessionUsage {
            by_model: self
                .by_model
                .iter()
                .map(|(model, counters)| {
                    (
                        model.clone(),
                        crate::SessionModelUsage {
                            input_tokens: counters.input_tokens,
                            output_tokens: counters.output_tokens,
                            cache_read_tokens: counters.cache_read_tokens,
                            cache_creation_tokens: counters.cache_creation_tokens,
                        },
                    )
                })
                .collect(),
            active_seconds: self.active_seconds,
            web_fetch_requests: self.web_fetch_requests,
            web_search_requests: self.web_search_requests,
            ..Default::default()
        };
        for counters in self.by_model.values() {
            usage.input_tokens = usage
                .input_tokens
                .checked_add(counters.input_tokens)
                .ok_or_else(list_cost_overflow)?;
            usage.output_tokens = usage
                .output_tokens
                .checked_add(counters.output_tokens)
                .ok_or_else(list_cost_overflow)?;
            usage.cache_read_tokens = usage
                .cache_read_tokens
                .checked_add(counters.cache_read_tokens)
                .ok_or_else(list_cost_overflow)?;
            usage.cache_creation_tokens = usage
                .cache_creation_tokens
                .checked_add(counters.cache_creation_tokens)
                .ok_or_else(list_cost_overflow)?;
        }
        Ok(usage)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedModelUsageCursor {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// One durable admissible→reached transition of the shared Session budget.
///
/// This is provenance on the existing aggregate ledger, not a child/Run
/// terminal override: Thread lifecycle keeps its own Ended/Awaiting truth.
/// The exact cumulative cursor and frozen price identity let every protocol
/// projection reproduce the usage immediately preceding `budget_reached`, even
/// after the cap is raised or removed and after a cold restart.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetReachTransition {
    pub generation: u64,
    pub max_list_cost_minor: u64,
    #[serde(with = "u128_decimal")]
    pub consumed_numerator: u128,
    pub usage_cursor: ManagedBudgetUsageCursor,
    pub price_snapshot_id: String,
}

impl BudgetReachTransition {
    #[must_use]
    pub fn public_list_cost_minor(&self) -> Option<u64> {
        public_list_cost_minor(self.consumed_numerator).ok()
    }
}

/// Durable budget lifecycle. `Removed` is distinct from `Absent` because the
/// official API permits one-way removal but does not permit adding a budget to
/// a Session that was created without one or re-adding a removed budget.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionBudgetState {
    #[default]
    Absent,
    Active {
        max_list_cost_minor: u64,
        #[serde(with = "u128_decimal")]
        consumed_numerator: u128,
        usage_cursor: ManagedBudgetUsageCursor,
        snapshot: ManagedListPriceSnapshot,
        #[serde(default)]
        reach_transitions: Vec<BudgetReachTransition>,
    },
    Removed {
        #[serde(with = "u128_decimal")]
        consumed_numerator: u128,
        usage_cursor: ManagedBudgetUsageCursor,
        snapshot: ManagedListPriceSnapshot,
        #[serde(default)]
        reach_transitions: Vec<BudgetReachTransition>,
    },
}

mod u128_decimal {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl SessionBudgetState {
    pub const COST_DENOMINATOR: u128 = 1_000_000;
    pub const MICROS_PER_MINOR_USD: u128 = 10_000;

    pub fn active(max_list_cost_minor: u64, snapshot: ManagedListPriceSnapshot) -> Self {
        Self::Active {
            max_list_cost_minor,
            consumed_numerator: 0,
            usage_cursor: ManagedBudgetUsageCursor::default(),
            snapshot,
            reach_transitions: Vec::new(),
        }
    }

    #[must_use]
    pub fn can_admit_model_request(&self) -> bool {
        match self {
            Self::Active {
                max_list_cost_minor,
                consumed_numerator,
                ..
            } => {
                *consumed_numerator
                    < u128::from(*max_list_cost_minor)
                        * Self::MICROS_PER_MINOR_USD
                        * Self::COST_DENOMINATOR
            }
            Self::Absent | Self::Removed { .. } => true,
        }
    }

    #[must_use]
    pub fn max_list_cost_minor(&self) -> Option<u64> {
        match self {
            Self::Active {
                max_list_cost_minor,
                ..
            } => Some(*max_list_cost_minor),
            Self::Absent | Self::Removed { .. } => None,
        }
    }

    /// The immutable pricing authority frozen for this Session. `Absent`
    /// intentionally has no pricing snapshot; Managed may then omit optional
    /// list-cost fields rather than resolving mutable current prices later.
    #[must_use]
    pub fn price_snapshot(&self) -> Option<&ManagedListPriceSnapshot> {
        match self {
            Self::Active { snapshot, .. } | Self::Removed { snapshot, .. } => Some(snapshot),
            Self::Absent => None,
        }
    }

    #[must_use]
    pub fn usage_cursor(&self) -> Option<&ManagedBudgetUsageCursor> {
        match self {
            Self::Active { usage_cursor, .. } | Self::Removed { usage_cursor, .. } => {
                Some(usage_cursor)
            }
            Self::Absent => None,
        }
    }

    /// Ordered cap-transition provenance retained across cap raises/removal.
    #[must_use]
    pub fn reach_transitions(&self) -> &[BudgetReachTransition] {
        match self {
            Self::Active {
                reach_transitions, ..
            }
            | Self::Removed {
                reach_transitions, ..
            } => reach_transitions,
            Self::Absent => &[],
        }
    }

    /// Append the next cap transition after cumulative usage has crossed the
    /// active threshold. Exact reconciliation replay is a no-op; raising the cap
    /// permits a later crossing to append the next generation.
    pub fn record_reach_transition(
        &mut self,
    ) -> Result<Option<BudgetReachTransition>, ManagedListPriceError> {
        let Self::Active {
            max_list_cost_minor,
            consumed_numerator,
            usage_cursor,
            snapshot,
            reach_transitions,
        } = self
        else {
            return Ok(None);
        };
        let threshold = u128::from(*max_list_cost_minor)
            .checked_mul(Self::MICROS_PER_MINOR_USD)
            .and_then(|value| value.checked_mul(Self::COST_DENOMINATOR))
            .ok_or_else(list_cost_overflow)?;
        if *consumed_numerator < threshold {
            return Ok(None);
        }
        if reach_transitions.last().is_some_and(|transition| {
            transition.max_list_cost_minor == *max_list_cost_minor
                && transition.consumed_numerator == *consumed_numerator
                && transition.usage_cursor == *usage_cursor
                && transition.price_snapshot_id == snapshot.snapshot_id
        }) {
            return Ok(None);
        }
        let generation = reach_transitions.last().map_or(Ok(1), |transition| {
            transition
                .generation
                .checked_add(1)
                .ok_or_else(list_cost_overflow)
        })?;
        let transition = BudgetReachTransition {
            generation,
            max_list_cost_minor: *max_list_cost_minor,
            consumed_numerator: *consumed_numerator,
            usage_cursor: usage_cursor.clone(),
            price_snapshot_id: snapshot.snapshot_id.clone(),
        };
        reach_transitions.push(transition.clone());
        Ok(Some(transition))
    }

    pub fn reconcile_cumulative_usage(
        &mut self,
        next: ManagedBudgetUsageCursor,
    ) -> Result<bool, ManagedListPriceError> {
        let (consumed_numerator, cursor, snapshot) = match self {
            Self::Active {
                consumed_numerator,
                usage_cursor,
                snapshot,
                ..
            }
            | Self::Removed {
                consumed_numerator,
                usage_cursor,
                snapshot,
                ..
            } => (consumed_numerator, usage_cursor, snapshot),
            Self::Absent => return Ok(false),
        };
        if !next.is_at_least(cursor) {
            return Err(ManagedListPriceError::InvalidSnapshot(
                "cumulative usage cannot move backwards".into(),
            ));
        }
        let delta = ManagedBudgetUsageCursor {
            by_model: next
                .by_model
                .iter()
                .map(|(model, current)| {
                    let previous = cursor.by_model.get(model).copied().unwrap_or_default();
                    (
                        model.clone(),
                        ManagedModelUsageCursor {
                            input_tokens: current.input_tokens - previous.input_tokens,
                            output_tokens: current.output_tokens - previous.output_tokens,
                            cache_read_tokens: current.cache_read_tokens
                                - previous.cache_read_tokens,
                            cache_creation_tokens: current.cache_creation_tokens
                                - previous.cache_creation_tokens,
                        },
                    )
                })
                .collect(),
            active_seconds: next.active_seconds - cursor.active_seconds,
            web_fetch_requests: next.web_fetch_requests - cursor.web_fetch_requests,
            web_search_requests: next.web_search_requests - cursor.web_search_requests,
        };
        let added = snapshot.usage_cost_numerator(delta)?;
        *consumed_numerator = consumed_numerator
            .checked_add(added)
            .ok_or_else(|| ManagedListPriceError::InvalidSnapshot("list cost overflow".into()))?;
        *cursor = next;
        Ok(added != 0)
    }
}

fn public_list_cost_minor(numerator: u128) -> Result<u64, ManagedListPriceError> {
    let denominator =
        SessionBudgetState::MICROS_PER_MINOR_USD * SessionBudgetState::COST_DENOMINATOR;
    let whole = numerator / denominator;
    let remainder = numerator % denominator;
    let rounded = whole
        .checked_add(u128::from(remainder >= denominator.div_ceil(2)))
        .ok_or_else(list_cost_overflow)?;
    u64::try_from(rounded)
        .map_err(|_| ManagedListPriceError::InvalidSnapshot("list cost overflow".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ManagedListPriceSnapshot {
        ManagedListPriceSnapshot {
            snapshot_id: "managed-list-1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: BTreeMap::from([(
                "model-a".into(),
                ManagedTokenListRates {
                    input_micros_per_million: 3_000_000,
                    output_micros_per_million: 15_000_000,
                    cache_read_micros_per_million: 300_000,
                    cache_creation_micros_per_million: 3_750_000,
                },
            )]),
            runtime_rates: ManagedRuntimeListRates {
                active_micros_per_million_seconds: 1_000_000,
                web_fetch_micros_per_request: 10,
                web_search_micros_per_request: 20,
            },
            fingerprint: "price-fingerprint".into(),
        }
    }

    #[test]
    fn budget_admission_and_replay_follow_the_exact_cost_decision_table() {
        // Cause/effect graph: C1 active budget, C2 cumulative usage advances,
        // C3 exact replay, C4 exact cost reaches the cap. Effects: E1 add only
        // the delta, E2 replay has no monetary effect, E3 later admission stops.
        // Decision table: R1=C1+C2 -> E1; R2=C1+C3 -> E2; R3=C1+C4 -> E3.
        let mut budget = SessionBudgetState::active(1, snapshot());
        let usage = ManagedBudgetUsageCursor {
            by_model: BTreeMap::from([(
                "model-a".into(),
                ManagedModelUsageCursor {
                    input_tokens: 3_334,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert!(
            budget.reconcile_cumulative_usage(usage.clone()).unwrap(),
            "R1"
        );
        assert!(!budget.reconcile_cumulative_usage(usage).unwrap(), "R2");
        assert!(!budget.can_admit_model_request(), "R3");
    }

    #[test]
    fn cap_transition_provenance_is_append_only_across_replay_and_raise() {
        // Cause/effect graph: C1 cumulative usage crosses an active cap; C2 the
        // exact cursor is reconciled/reported again; C3 the cap is raised above
        // consumption and later crossed; C4 the transition cursor is converted
        // back to neutral usage. Effects: E1 append generation 1 with exact
        // price/usage coordinates; E2 append nothing; E3 append generation 2
        // without erasing generation 1; E4 reproduce cumulative counters.
        //
        // | Rule | Crossing | Cursor | Cap | Effect |
        // |---|---|---|---|---|
        // | T1 | first | advances | 1 | E1+E4 |
        // | T2 | replay | same | 1 | E2 |
        // | T3 | second | advances | raised to 2 | E3 |
        // Constraints/invariants: transition provenance is append-only and
        // idempotent for one cursor/cap generation; raising never rewrites T1.
        let mut budget = SessionBudgetState::active(1, snapshot());
        let first = ManagedBudgetUsageCursor {
            by_model: BTreeMap::from([(
                "model-a".into(),
                ManagedModelUsageCursor {
                    input_tokens: 3_334,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        budget.reconcile_cumulative_usage(first).unwrap();
        let transition = budget.record_reach_transition().unwrap().expect("T1/E1");
        assert_eq!(transition.generation, 1, "T1/E1");
        assert_eq!(transition.price_snapshot_id, "managed-list-1", "T1/E1");
        assert_eq!(
            transition
                .usage_cursor
                .to_session_usage()
                .unwrap()
                .input_tokens,
            3_334,
            "T1/E4"
        );
        assert!(budget.record_reach_transition().unwrap().is_none(), "T2/E2");
        let SessionBudgetState::Active {
            max_list_cost_minor,
            ..
        } = &mut budget
        else {
            unreachable!()
        };
        *max_list_cost_minor = 2;
        budget
            .reconcile_cumulative_usage(ManagedBudgetUsageCursor {
                by_model: BTreeMap::from([(
                    "model-a".into(),
                    ManagedModelUsageCursor {
                        input_tokens: 6_668,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            budget
                .record_reach_transition()
                .unwrap()
                .expect("T3/E3")
                .generation,
            2,
            "T3/E3"
        );
        assert_eq!(budget.reach_transitions().len(), 2, "T3/E3");
    }

    #[test]
    fn list_cost_overflow_is_rejected_without_panicking_or_wrapping() {
        // Test design — Causes: C1 maximal token counts multiply/add against
        // maximal frozen model rates; C2 a maximal runtime request count
        // multiplies a maximal request rate. Effects: both return the stable
        // InvalidSnapshot overflow error without panic or wrapped cost.
        // Constraints/invariants: all list-cost arithmetic is checked before
        // public rounding. Decision rules O1=C1=>error; O2=C2=>same error cover
        // the model-usage and runtime-usage accumulation paths independently.
        let mut snapshot = snapshot();
        snapshot.model_rates.insert(
            "overflow".into(),
            ManagedTokenListRates {
                input_micros_per_million: u64::MAX,
                output_micros_per_million: u64::MAX,
                cache_read_micros_per_million: u64::MAX,
                cache_creation_micros_per_million: u64::MAX,
            },
        );
        let usage = ManagedBudgetUsageCursor {
            by_model: BTreeMap::from([(
                "overflow".into(),
                ManagedModelUsageCursor {
                    input_tokens: u64::MAX,
                    output_tokens: u64::MAX,
                    cache_read_tokens: u64::MAX,
                    cache_creation_tokens: u64::MAX,
                },
            )]),
            ..Default::default()
        };
        assert_eq!(
            snapshot.usage_cost_numerator(usage),
            Err(ManagedListPriceError::InvalidSnapshot(
                "list cost overflow".into()
            ))
        );

        snapshot.model_rates.clear();
        snapshot.runtime_rates.web_fetch_micros_per_request = u64::MAX;
        assert_eq!(
            snapshot.usage_cost_numerator(ManagedBudgetUsageCursor {
                web_fetch_requests: u64::MAX,
                ..Default::default()
            }),
            Err(ManagedListPriceError::InvalidSnapshot(
                "list cost overflow".into()
            ))
        );
    }

    #[test]
    fn session_and_thread_public_costs_share_arithmetic_but_round_independently() {
        // Cause/effect graph: C1 one frozen price snapshot; C2 a public cost is
        // below/at/above half of one minor unit or exceeds the public u64 wire;
        // C3 two logical Threads each
        // consume just over half a unit; C4 the Session prices their combined
        // cumulative usage. E1 round down/half-up/up at the public projection
        // boundary; E2 round each Thread independently; E3 price and round the
        // Session aggregate only once; E4 Active/Removed expose the frozen
        // snapshot and Absent does not invent one.
        //
        // | Rule | Amount | Scope | Effect |
        // |---|---|---|---|
        // | P1 | below half / exact half / above half | direct | 0 / 1 / 1 |
        // | P1b | exceeds u64 | direct | overflow error |
        // | P2 | just over half each | two Threads | 1 + 1 |
        // | P3 | just over one unit total | Session | 1 |
        // | P4 | any | Active/Removed/Absent | snapshot/snapshot/none |
        // Constraints/invariants: all scopes share frozen integer arithmetic,
        // but rounding occurs only at each scope's public projection boundary.
        let snapshot = snapshot();
        let denominator =
            SessionBudgetState::MICROS_PER_MINOR_USD * SessionBudgetState::COST_DENOMINATOR;
        assert_eq!(
            public_list_cost_minor(denominator / 2 - 1).unwrap(),
            0,
            "P1/E1 below half"
        );
        assert_eq!(
            public_list_cost_minor(denominator / 2).unwrap(),
            1,
            "P1/E1 exact half rounds up"
        );
        assert_eq!(
            public_list_cost_minor(denominator / 2 + 1).unwrap(),
            1,
            "P1/E1 above half"
        );
        assert_eq!(
            public_list_cost_minor(u128::MAX),
            Err(ManagedListPriceError::InvalidSnapshot(
                "list cost overflow".into()
            )),
            "P1b/E1 fails closed"
        );
        let per_thread = ManagedBudgetUsageCursor {
            by_model: BTreeMap::from([(
                "model-a".into(),
                ManagedModelUsageCursor {
                    input_tokens: 1_667,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert_eq!(
            snapshot
                .public_usage_list_cost_minor(per_thread.clone())
                .unwrap(),
            1,
            "P2/E2 first Thread"
        );
        assert_eq!(
            snapshot.public_usage_list_cost_minor(per_thread).unwrap(),
            1,
            "P2/E2 second Thread"
        );
        let aggregate = ManagedBudgetUsageCursor {
            by_model: BTreeMap::from([(
                "model-a".into(),
                ManagedModelUsageCursor {
                    input_tokens: 3_334,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert_eq!(
            snapshot.public_usage_list_cost_minor(aggregate).unwrap(),
            1,
            "P3/E3"
        );
        let active = SessionBudgetState::active(10, snapshot.clone());
        assert_eq!(active.price_snapshot(), Some(&snapshot), "P4/E4 active");
        let removed = SessionBudgetState::Removed {
            consumed_numerator: 0,
            usage_cursor: ManagedBudgetUsageCursor::default(),
            snapshot: snapshot.clone(),
            reach_transitions: Vec::new(),
        };
        assert_eq!(removed.price_snapshot(), Some(&snapshot), "P4/E4 removed");
        assert!(
            SessionBudgetState::Absent.price_snapshot().is_none(),
            "P4/E4 absent"
        );
    }

    #[test]
    fn neutral_usage_has_one_model_attribution_conversion_owner() {
        // Cause/effect graph: C1 usage already carries served-model buckets;
        // C2 legacy token usage has no bucket but a frozen fallback exists; C3
        // neither attribution source exists. E1 preserves exact per-model and
        // non-token counters; E2 attributes all legacy tokens to the frozen
        // model; E3 fails closed before pricing. Rules U1=C1=>E1,
        // U2=!C1+C2=>E2, U3=!C1+!C2+C3=>E3.
        // Constraints/invariants: existing per-model attribution is never
        // rewritten, and unattributed legacy tokens require one frozen fallback.
        let attributed = crate::SessionUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_tokens: 5,
            cache_creation_tokens: 3,
            by_model: BTreeMap::from([(
                "served".into(),
                crate::SessionModelUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_read_tokens: 5,
                    cache_creation_tokens: 3,
                },
            )]),
            active_seconds: 2,
            web_fetch_requests: 1,
            web_search_requests: 4,
        };
        let cursor = ManagedBudgetUsageCursor::from_session_usage(&attributed, None).unwrap();
        assert_eq!(cursor.by_model["served"].cache_creation_tokens, 3, "U1/E1");
        assert_eq!(cursor.active_seconds, 2, "U1/E1");
        assert_eq!(cursor.web_search_requests, 4, "U1/E1");

        let mut legacy = attributed.clone();
        legacy.by_model.clear();
        let cursor = ManagedBudgetUsageCursor::from_session_usage(&legacy, Some("frozen")).unwrap();
        assert_eq!(cursor.by_model["frozen"].input_tokens, 11, "U2/E2");
        assert!(
            ManagedBudgetUsageCursor::from_session_usage(&legacy, None).is_err(),
            "U3/E3"
        );
    }
}
