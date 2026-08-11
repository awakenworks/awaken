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
                    u128::from(model_usage.input_tokens)
                        * u128::from(rates.input_micros_per_million)
                        + u128::from(model_usage.output_tokens)
                            * u128::from(rates.output_micros_per_million)
                        + u128::from(model_usage.cache_read_tokens)
                            * u128::from(rates.cache_read_micros_per_million)
                        + u128::from(model_usage.cache_creation_tokens)
                            * u128::from(rates.cache_creation_micros_per_million),
                )
                .ok_or_else(|| {
                    ManagedListPriceError::InvalidSnapshot("list cost overflow".into())
                })?;
        }
        cost = cost
            .checked_add(
                u128::from(usage.active_seconds)
                    * u128::from(self.runtime_rates.active_micros_per_million_seconds),
            )
            .and_then(|value| {
                value.checked_add(
                    u128::from(usage.web_fetch_requests)
                        * u128::from(self.runtime_rates.web_fetch_micros_per_request)
                        * Self::COST_DENOMINATOR,
                )
            })
            .and_then(|value| {
                value.checked_add(
                    u128::from(usage.web_search_requests)
                        * u128::from(self.runtime_rates.web_search_micros_per_request)
                        * Self::COST_DENOMINATOR,
                )
            })
            .ok_or_else(|| ManagedListPriceError::InvalidSnapshot("list cost overflow".into()))?;
        Ok(cost)
    }

    const COST_DENOMINATOR: u128 = 1_000_000;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedModelUsageCursor {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
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
        reached_event_emitted: bool,
    },
    Removed {
        #[serde(with = "u128_decimal")]
        consumed_numerator: u128,
        usage_cursor: ManagedBudgetUsageCursor,
        snapshot: ManagedListPriceSnapshot,
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
            reached_event_emitted: false,
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

    #[must_use]
    pub fn public_list_cost_minor(&self) -> Option<u64> {
        let numerator = match self {
            Self::Active {
                consumed_numerator, ..
            }
            | Self::Removed {
                consumed_numerator, ..
            } => *consumed_numerator,
            Self::Absent => return None,
        };
        let minor_denominator = Self::MICROS_PER_MINOR_USD * Self::COST_DENOMINATOR;
        u64::try_from(numerator / minor_denominator).ok()
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
            } => (consumed_numerator, usage_cursor, snapshot),
            Self::Absent => return Ok(false),
        };
        if next.active_seconds < cursor.active_seconds
            || next.web_fetch_requests < cursor.web_fetch_requests
            || next.web_search_requests < cursor.web_search_requests
            || cursor.by_model.iter().any(|(model, previous)| {
                let current = next.by_model.get(model).copied().unwrap_or_default();
                current.input_tokens < previous.input_tokens
                    || current.output_tokens < previous.output_tokens
                    || current.cache_read_tokens < previous.cache_read_tokens
                    || current.cache_creation_tokens < previous.cache_creation_tokens
            })
        {
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
}
