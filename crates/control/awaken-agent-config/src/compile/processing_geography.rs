//! Atomic processing-geography admission for one resolved model attempt pool.

use awaken_runtime_contract::agent_bindings::InferenceGeography;
use awaken_runtime_contract::resolved::{
    InferencePlacementMechanism, ModelProvisioning, ResolvedModelCandidate,
};

use super::CompileError;

#[must_use]
fn candidate_geography_admitted(
    required: Option<InferenceGeography>,
    provider_candidate: bool,
    placement: Option<awaken_runtime_contract::resolved::InferencePlacement>,
    anthropic_messages: bool,
) -> bool {
    let Some(required) = required else {
        return true;
    };
    let Some(placement) = placement else {
        return false;
    };
    provider_candidate
        && placement.geography == required
        && (placement.mechanism != InferencePlacementMechanism::AnthropicRequestBody
            || (required == InferenceGeography::Us && anthropic_messages))
}

pub(super) fn validate_candidate_pool(
    required: Option<InferenceGeography>,
    agent: &str,
    primary: &ResolvedModelCandidate,
    fallbacks: &[ResolvedModelCandidate],
    advisor: Option<&ResolvedModelCandidate>,
) -> Result<(), CompileError> {
    let Some(required) = required else {
        return Ok(());
    };
    for candidate in std::iter::once(primary).chain(fallbacks).chain(advisor) {
        let ModelProvisioning::Provider { endpoint, .. } = &candidate.provisioning else {
            debug_assert!(!candidate_geography_admitted(
                Some(required),
                false,
                None,
                false
            ));
            return Err(invalid(
                agent,
                format!(
                    "model candidate {:?} cannot prove inference_geo `{required}`",
                    candidate.binding
                ),
            ));
        };
        if candidate_geography_admitted(
            Some(required),
            true,
            endpoint.processing_placement,
            endpoint.api_dialect == "anthropic_messages",
        ) {
            continue;
        }
        let Some(placement) = endpoint.processing_placement else {
            return Err(invalid(
                agent,
                format!(
                    "model candidate {:?} has no processing placement for inference_geo `{required}`",
                    candidate.binding
                ),
            ));
        };
        if placement.geography != required {
            return Err(invalid(
                agent,
                format!(
                    "model candidate {:?} processing geography does not satisfy inference_geo `{required}`",
                    candidate.binding
                ),
            ));
        }
        return Err(invalid(
            agent,
            format!(
                "model candidate {:?} uses Anthropic placement on protocol `{}`",
                candidate.binding, endpoint.api_dialect
            ),
        ));
    }
    Ok(())
}

#[cfg(kani)]
fn arbitrary_geography(value: u8) -> InferenceGeography {
    match value % 9 {
        0 => InferenceGeography::Us,
        1 => InferenceGeography::Eu,
        2 => InferenceGeography::Apac,
        3 => InferenceGeography::Cn,
        4 => InferenceGeography::Jp,
        5 => InferenceGeography::Au,
        6 => InferenceGeography::Ca,
        7 => InferenceGeography::Uk,
        _ => InferenceGeography::Hk,
    }
}

#[cfg(kani)]
#[kani::proof]
fn processing_geography_requires_exact_evidence_from_every_candidate() {
    let required = kani::any::<bool>().then(|| arbitrary_geography(kani::any()));
    let provider_candidate = kani::any();
    let placement =
        kani::any::<bool>().then(|| awaken_runtime_contract::resolved::InferencePlacement {
            geography: arbitrary_geography(kani::any()),
            mechanism: if kani::any() {
                InferencePlacementMechanism::AnthropicRequestBody
            } else {
                InferencePlacementMechanism::FrozenRegionalRoute
            },
        });
    let anthropic_messages = kani::any();
    let admitted =
        candidate_geography_admitted(required, provider_candidate, placement, anthropic_messages);
    let expected = required.is_none()
        || required.is_some_and(|required| {
            provider_candidate
                && placement.is_some_and(|placement| {
                    placement.geography == required
                        && (placement.mechanism
                            != InferencePlacementMechanism::AnthropicRequestBody
                            || (required == InferenceGeography::Us && anthropic_messages))
                })
        });
    assert_eq!(admitted, expected);
}

fn invalid(agent: &str, reason: String) -> CompileError {
    CompileError::InvalidResolvedModels {
        agent: agent.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use awaken_runtime_contract::resolved::{
        InferenceEndpoint, InferencePlacement, ModelBinding, ResolvedModelCandidate,
    };

    use super::*;

    fn candidate(
        model: &str,
        dialect: &str,
        processing_placement: Option<InferencePlacement>,
    ) -> ResolvedModelCandidate {
        ResolvedModelCandidate::provider(
            ModelBinding::new("provider", model, "gateway"),
            "provider-account",
            format!("route:1:{model}"),
            "workspace-a",
            None,
            InferenceEndpoint {
                adapter_kind: dialect.into(),
                api_dialect: dialect.into(),
                base_url: "https://provider.example.test".into(),
                upstream_model: model.into(),
                processing_placement,
            },
        )
    }

    #[test]
    fn requirement_applies_to_the_whole_model_pool_and_native_mechanism() {
        // Cause/effect graph: C1=optional exact requirement, C2=matching proof,
        // C3=compatible mechanism/protocol, C4=every candidate proved. E1 admits
        // one immutable pool; E2 rejects before any snapshot publication.
        //
        // | Rule | req | primary proof | fallback proof | protocol | effect |
        // | P1   | no  | any           | any            | any      | E1     |
        // | P2   | us  | frozen/us     | frozen/us      | OpenAI   | E1     |
        // | P4a  | us  | body/us       | n/a            | OpenAI   | E2     |
        // | P4b  | eu  | body/eu       | n/a            | Anthropic| E2     |
        // | P5   | us  | frozen/us     | absent         | OpenAI   | E2     |
        let placement = |geography, mechanism| {
            Some(InferencePlacement {
                geography,
                mechanism,
            })
        };
        let frozen_us = placement(
            InferenceGeography::Us,
            InferencePlacementMechanism::FrozenRegionalRoute,
        );
        let primary = candidate("primary", "open_ai_chat", frozen_us);
        let fallback = candidate("fallback", "open_ai_chat", frozen_us);
        assert!(
            validate_candidate_pool(None, "agent", &primary, &[], None).is_ok(),
            "P1"
        );
        assert!(
            validate_candidate_pool(
                Some(InferenceGeography::Us),
                "agent",
                &primary,
                &[fallback],
                None,
            )
            .is_ok(),
            "P2"
        );
        for (rule, requirement, invalid) in [
            (
                "P4a",
                InferenceGeography::Us,
                candidate(
                    "body-us",
                    "open_ai_chat",
                    placement(
                        InferenceGeography::Us,
                        InferencePlacementMechanism::AnthropicRequestBody,
                    ),
                ),
            ),
            (
                "P4b",
                InferenceGeography::Eu,
                candidate(
                    "body-eu",
                    "anthropic_messages",
                    placement(
                        InferenceGeography::Eu,
                        InferencePlacementMechanism::AnthropicRequestBody,
                    ),
                ),
            ),
        ] {
            assert!(
                validate_candidate_pool(Some(requirement), "agent", &invalid, &[], None).is_err(),
                "{rule}"
            );
        }
        assert!(
            validate_candidate_pool(
                Some(InferenceGeography::Us),
                "agent",
                &primary,
                &[candidate("missing", "open_ai_chat", None)],
                None,
            )
            .is_err(),
            "P5"
        );
    }
}
