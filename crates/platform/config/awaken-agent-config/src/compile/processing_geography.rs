// @formal entrypoint model_geography.every_attempt_candidate_requires_exact_processing_evidence
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
        let ModelProvisioning::Provider { endpoint, .. } = candidate.provisioning() else {
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
                    candidate.binding()
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
                    candidate.binding()
                ),
            ));
        };
        if placement.geography != required {
            return Err(invalid(
                agent,
                format!(
                    "model candidate {:?} processing geography does not satisfy inference_geo `{required}`",
                    candidate.binding()
                ),
            ));
        }
        return Err(invalid(
            agent,
            format!(
                "model candidate {:?} uses Anthropic placement on protocol `{}`",
                candidate.binding(),
                endpoint.api_dialect
            ),
        ));
    }
    Ok(())
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
        ResolvedModelCandidate::try_provider(
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
        .expect("coherent geography provider candidate")
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

#[cfg(test)]
mod exhaustive {
    use super::*;
    use awaken_runtime_contract::resolved::InferencePlacement;

    const fn listed_geography(geography: InferenceGeography) -> usize {
        match geography {
            InferenceGeography::Us => 0,
            InferenceGeography::Eu => 1,
            InferenceGeography::Apac => 2,
            InferenceGeography::Cn => 3,
            InferenceGeography::Jp => 4,
            InferenceGeography::Au => 5,
            InferenceGeography::Ca => 6,
            InferenceGeography::Uk => 7,
            InferenceGeography::Hk => 8,
        }
    }
    const EVERY_GEOGRAPHY: [InferenceGeography; 9] = [
        InferenceGeography::Us,
        InferenceGeography::Eu,
        InferenceGeography::Apac,
        InferenceGeography::Cn,
        InferenceGeography::Jp,
        InferenceGeography::Au,
        InferenceGeography::Ca,
        InferenceGeography::Uk,
        InferenceGeography::Hk,
    ];

    const fn listed_mechanism(mechanism: InferencePlacementMechanism) -> usize {
        match mechanism {
            InferencePlacementMechanism::AnthropicRequestBody => 0,
            InferencePlacementMechanism::FrozenRegionalRoute => 1,
        }
    }
    const EVERY_MECHANISM: [InferencePlacementMechanism; 2] = [
        InferencePlacementMechanism::AnthropicRequestBody,
        InferencePlacementMechanism::FrozenRegionalRoute,
    ];

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_GEOGRAPHY.into_iter().enumerate() {
            assert_eq!(listed_geography(value), index);
        }
        for (index, value) in EVERY_MECHANISM.into_iter().enumerate() {
            assert_eq!(listed_mechanism(value), index);
        }
    }

    /// @exhaustive Option<InferenceGeography>(10) x bool(2) x Option<InferenceGeography(9) x InferencePlacementMechanism(2)>(19) x bool(2) = 760
    /// @obligation model_geography.every_attempt_candidate_requires_exact_processing_evidence
    #[test]
    fn processing_geography_requires_exact_evidence_from_every_candidate() {
        let requireds = std::iter::once(None).chain(EVERY_GEOGRAPHY.map(Some));
        let placements: Vec<Option<InferencePlacement>> = std::iter::once(None)
            .chain(EVERY_GEOGRAPHY.into_iter().flat_map(|geography| {
                EVERY_MECHANISM.map(|mechanism| {
                    Some(InferencePlacement {
                        geography,
                        mechanism,
                    })
                })
            }))
            .collect();
        assert_eq!(placements.len(), 19);
        for required in requireds {
            for provider_candidate in [false, true] {
                for placement in placements.iter().copied() {
                    for anthropic_messages in [false, true] {
                        let admitted = candidate_geography_admitted(
                            required,
                            provider_candidate,
                            placement,
                            anthropic_messages,
                        );
                        let expected = required.is_none()
                            || required.is_some_and(|required| {
                                provider_candidate
                                    && placement.is_some_and(|placement| {
                                        placement.geography == required
                                            && (placement.mechanism
                                                != InferencePlacementMechanism::AnthropicRequestBody
                                                || (required == InferenceGeography::Us
                                                    && anthropic_messages))
                                    })
                            });
                        assert_eq!(admitted, expected);
                    }
                }
            }
        }
    }
}
