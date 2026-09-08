use awaken_runtime_contract::llm::{Error, Result, StopReason};

// The SDK's raw strings have already been classified by its enum variant. Keep
// the production decision kernel free of payload ownership and allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
enum ProviderStopClass {
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

fn project_stop_class(class: ProviderStopClass) -> Option<StopReason> {
    match class {
        ProviderStopClass::Completed => Some(StopReason::NaturalEnd),
        ProviderStopClass::MaxTokens => Some(StopReason::MaxTokens),
        ProviderStopClass::ToolCall => Some(StopReason::ToolUse),
        ProviderStopClass::StopSequence => Some(StopReason::StopSequence),
        ProviderStopClass::ContentFilter => Some(StopReason::ContentFilter),
        ProviderStopClass::Unclassified => None,
    }
}

/// Map the SDK's stop reason onto the neutral one. A provider-specific reason
/// the SDK cannot classify (`Other`) fails at this adapter boundary instead of
/// being erased into `None` and mistaken for a natural end by Runtime.
pub fn map_stop_reason(reason: &genai::chat::StopReason) -> Result<StopReason> {
    project_stop_class(provider_stop_class(reason))
        .ok_or_else(|| Error::Provider("model returned an unclassified stop reason".into()))
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn unclassified_provider_stop_reason_fails_closed() {
        // Cause/effect graph: C1-C5 are the five classified provider stop
        // classes and C6 is Unclassified. E1-E5 are the exact neutral stop
        // reasons; E6 is no projection, which the public adapter converts to a
        // Provider error. Constraint: raw provider strings carry no additional
        // classification authority. Decision table K1-K6 exhausts both the SDK
        // variant boundary and derived six-variant production kernel without
        // modeling the unrelated allocation of the final diagnostic message.
        let class: ProviderStopClass = kani::any();
        let sdk_reason = match class {
            ProviderStopClass::Completed => genai::chat::StopReason::Completed(String::new()),
            ProviderStopClass::MaxTokens => genai::chat::StopReason::MaxTokens(String::new()),
            ProviderStopClass::ToolCall => genai::chat::StopReason::ToolCall(String::new()),
            ProviderStopClass::StopSequence => genai::chat::StopReason::StopSequence(String::new()),
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
