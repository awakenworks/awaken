use awaken_runtime_contract::resolved::{AcpSessionConfiguration, ModelBinding};
use awaken_runtime_host::PublicationResolutionError;

pub(super) fn validate_acp_session_configuration(
    binding: &ModelBinding,
    configuration: &AcpSessionConfiguration,
    negotiated: &awaken_acp_contract::NegotiatedAcpCapabilities,
) -> Result<(), PublicationResolutionError> {
    let unavailable = |reason: String| PublicationResolutionError::CandidateUnavailable {
        binding: binding.clone(),
        reason,
    };
    if let Some(mode) = &configuration.mode
        && !negotiated
            .modes
            .iter()
            .any(|available| &available.native_id == mode)
    {
        return Err(unavailable(format!(
            "ACP mode `{mode}` is not advertised by {}",
            binding.backend_ref
        )));
    }
    for (id, value) in &configuration.options {
        let option = negotiated
            .config_options
            .iter()
            .find(|option| &option.native_id == id)
            .ok_or_else(|| {
                unavailable(format!(
                    "ACP option `{id}` is not advertised by {}",
                    binding.backend_ref
                ))
            })?;
        if !option
            .choices
            .iter()
            .any(|choice| &choice.native_value == value)
        {
            return Err(unavailable(format!(
                "ACP option `{id}` does not advertise value `{value}`"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_configuration_is_validated_against_one_negotiated_profile() {
        // Cause graph: authored mode/options + the publication-pinned negotiated
        // profile -> immutable executable candidate or fail-closed publication.
        //
        // Decision table:
        // C1 omitted configuration                  -> valid
        // C2 advertised mode + option/value         -> valid
        // C3 missing mode                           -> unavailable
        // C4 missing option                         -> unavailable
        // C5 option exists but value not advertised -> unavailable
        let binding = ModelBinding::new("", "", "acp:codex");
        let negotiated = awaken_acp_contract::NegotiatedAcpCapabilities {
            protocol_version: "1".into(),
            load_session: false,
            prompt_image: false,
            prompt_audio: false,
            prompt_embedded_context: false,
            mcp_http: false,
            mcp_sse: false,
            session_list: false,
            modes: vec![awaken_acp_contract::AcpSessionModeDescriptor {
                native_id: "plan".into(),
                name: "Plan".into(),
                description: None,
                current: false,
            }],
            config_options: vec![awaken_acp_contract::AcpSessionConfigOptionDescriptor {
                native_id: "reasoning_effort".into(),
                name: "Reasoning effort".into(),
                description: None,
                category: None,
                current_value: "medium".into(),
                choices: vec![awaken_acp_contract::AcpSessionConfigChoice {
                    native_value: "high".into(),
                    name: "High".into(),
                    description: None,
                    group_id: None,
                    group_name: None,
                }],
            }],
        };
        assert!(
            validate_acp_session_configuration(&binding, &Default::default(), &negotiated).is_ok(),
            "C1"
        );
        let valid = AcpSessionConfiguration {
            mode: Some("plan".into()),
            options: [("reasoning_effort".into(), "high".into())]
                .into_iter()
                .collect(),
        };
        assert!(
            validate_acp_session_configuration(&binding, &valid, &negotiated).is_ok(),
            "C2"
        );
        let mut invalid = valid.clone();
        invalid.mode = Some("unknown".into());
        assert!(
            validate_acp_session_configuration(&binding, &invalid, &negotiated).is_err(),
            "C3"
        );
        invalid = valid.clone();
        invalid.options = [("unknown".into(), "high".into())].into_iter().collect();
        assert!(
            validate_acp_session_configuration(&binding, &invalid, &negotiated).is_err(),
            "C4"
        );
        invalid = valid;
        invalid
            .options
            .insert("reasoning_effort".into(), "impossible".into());
        assert!(
            validate_acp_session_configuration(&binding, &invalid, &negotiated).is_err(),
            "C5"
        );
    }
}
