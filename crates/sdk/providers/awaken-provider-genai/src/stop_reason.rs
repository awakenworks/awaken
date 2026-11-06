// @formal entrypoint model_stop.unclassified_provider_reason_fails_closed
use awaken_runtime_contract::llm::{Error, Result, StopReason};
use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, TerminalReceipt};

// Provider-specific wire values are classified before entering this finite
// kernel. Keep its neutral projection free of payload ownership and allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderStopClass {
    Completed,
    MaxTokens,
    ToolCall,
    StopSequence,
    ContentFilter,
    Unclassified,
}

fn provider_stop_class(reason: &genai::chat::StopReason) -> ProviderStopClass {
    use genai::chat::StopReason as GenaiStopReason;
    match reason {
        GenaiStopReason::Completed(_) => ProviderStopClass::Completed,
        GenaiStopReason::MaxTokens(_) => ProviderStopClass::MaxTokens,
        GenaiStopReason::ToolCall(_) => ProviderStopClass::ToolCall,
        GenaiStopReason::StopSequence(_) => ProviderStopClass::StopSequence,
        GenaiStopReason::ContentFilter(_) => ProviderStopClass::ContentFilter,
        GenaiStopReason::Other(_) => ProviderStopClass::Unclassified,
    }
}

pub(crate) const fn project_stop_class(class: ProviderStopClass) -> Option<StopReason> {
    match class {
        ProviderStopClass::Completed => Some(StopReason::NaturalEnd),
        ProviderStopClass::MaxTokens => Some(StopReason::MaxTokens),
        ProviderStopClass::ToolCall => Some(StopReason::ToolUse),
        ProviderStopClass::StopSequence => Some(StopReason::StopSequence),
        ProviderStopClass::ContentFilter => Some(StopReason::ContentFilter),
        ProviderStopClass::Unclassified => None,
    }
}

/// Interpret the terminal status fields of the OpenAI Responses dialect before
/// any returned output may enter Runtime. The adapter's raw string vocabulary
/// feeds the same finite stop-class projection as the SDK-backed dialect.
///
/// The status token is handed to [`TerminalReceipt::from_wire_status`] FIRST,
/// so an absent or unlisted status cannot reach a terminal class at all: it
/// leaves with an unannotated error whose evidence stays `Unknown`, and the
/// minted receipt is the only thing the rest of this dialect may attach.
pub(crate) fn map_responses_terminal_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> Result<(StopReason, TerminalReceipt)> {
    let receipt = TerminalReceipt::from_wire_status(status).map_err(|unproven| {
        Error::Provider(format!(
            "Responses operation has no confirmed terminal status: {unproven}"
        ))
    })?;
    let class = match status {
        Some("completed") => ProviderStopClass::Completed,
        Some("incomplete") => match incomplete_reason {
            Some("max_output_tokens") => ProviderStopClass::MaxTokens,
            Some("content_filter") => ProviderStopClass::ContentFilter,
            _ => ProviderStopClass::Unclassified,
        },
        Some("failed" | "cancelled") => {
            return Err(Error::Provider(format!(
                "Responses operation ended with status {status:?}"
            ))
            .with_execution_evidence(ModelExecutionEvidence::Terminated(receipt)));
        }
        // Unreachable today: minting already admitted exactly the tokens this
        // match classifies. It stays an explicit closed refusal so that adding
        // a token to the contract's table can only ever fail closed here,
        // never fall through into someone else's terminal class.
        _ => {
            return Err(Error::Provider(
                "Responses operation has no confirmed terminal status".into(),
            ));
        }
    };
    project_stop_class(class)
        .map(|stop| (stop, receipt))
        .ok_or_else(|| {
            // The receipt proves the operation ended, but an unclassified
            // reason cannot be guessed into Runtime's compatibility convention
            // that `None` means a natural end.
            Error::Provider("Responses operation returned an unclassified incomplete reason".into())
                .with_execution_evidence(ModelExecutionEvidence::Terminated(receipt))
        })
}

/// Map the SDK's stop reason onto the neutral one. A provider-specific reason
/// the SDK cannot classify (`Other`) fails at this adapter boundary instead of
/// being erased into `None` and mistaken for a natural end by Runtime.
pub fn map_stop_reason(reason: &genai::chat::StopReason) -> Result<StopReason> {
    project_stop_class(provider_stop_class(reason))
        .ok_or_else(|| Error::Provider("model returned an unclassified stop reason".into()))
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const fn listed_class(class: ProviderStopClass) -> usize {
        match class {
            ProviderStopClass::Completed => 0,
            ProviderStopClass::MaxTokens => 1,
            ProviderStopClass::ToolCall => 2,
            ProviderStopClass::StopSequence => 3,
            ProviderStopClass::ContentFilter => 4,
            ProviderStopClass::Unclassified => 5,
        }
    }
    const EVERY_CLASS: [ProviderStopClass; 6] = [
        ProviderStopClass::Completed,
        ProviderStopClass::MaxTokens,
        ProviderStopClass::ToolCall,
        ProviderStopClass::StopSequence,
        ProviderStopClass::ContentFilter,
        ProviderStopClass::Unclassified,
    ];

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_CLASS.into_iter().enumerate() {
            assert_eq!(listed_class(value), index);
        }
    }

    /// @exhaustive ProviderStopClass(6) = 6
    /// @obligation model_stop.unclassified_provider_reason_fails_closed
    #[test]
    fn unclassified_provider_stop_reason_fails_closed() {
        // Cause/effect graph: C1-C5 are the five classified provider stop
        // classes and C6 is Unclassified. E1-E5 are the exact neutral stop
        // reasons; E6 is no projection, which the public adapter converts to a
        // Provider error. Constraint: raw provider strings carry no additional
        // classification authority. Decision table K1-K6 exhausts both the SDK
        // variant boundary and derived six-variant production kernel without
        // modeling the unrelated allocation of the final diagnostic message.
        for class in EVERY_CLASS {
            let sdk_reason = match class {
                ProviderStopClass::Completed => genai::chat::StopReason::Completed(String::new()),
                ProviderStopClass::MaxTokens => genai::chat::StopReason::MaxTokens(String::new()),
                ProviderStopClass::ToolCall => genai::chat::StopReason::ToolCall(String::new()),
                ProviderStopClass::StopSequence => {
                    genai::chat::StopReason::StopSequence(String::new())
                }
                ProviderStopClass::ContentFilter => {
                    genai::chat::StopReason::ContentFilter(String::new())
                }
                ProviderStopClass::Unclassified => genai::chat::StopReason::Other(String::new()),
            };
            assert_eq!(provider_stop_class(&sdk_reason), class);
            let projected = project_stop_class(class);
            match class {
                ProviderStopClass::Completed => {
                    assert_eq!(projected, Some(StopReason::NaturalEnd));
                }
                ProviderStopClass::MaxTokens => {
                    assert_eq!(projected, Some(StopReason::MaxTokens));
                }
                ProviderStopClass::ToolCall => {
                    assert_eq!(projected, Some(StopReason::ToolUse));
                }
                ProviderStopClass::StopSequence => {
                    assert_eq!(projected, Some(StopReason::StopSequence));
                }
                ProviderStopClass::ContentFilter => {
                    assert_eq!(projected, Some(StopReason::ContentFilter));
                }
                ProviderStopClass::Unclassified => assert_eq!(projected, None),
            }
        }
    }
}
