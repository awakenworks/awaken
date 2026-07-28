//! Projection from the official ACP schema into Awaken's neutral capability
//! descriptors. This module performs no I/O and owns no discovery lifecycle.

use agent_client_protocol::{
    InitializeResponse, NewSessionResponse, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOptions, SessionModeState,
};

use crate::{
    AcpSessionConfigChoice, AcpSessionConfigOptionDescriptor, AcpSessionModeDescriptor,
    NegotiatedAcpCapabilities,
};

pub(crate) fn project(
    init: InitializeResponse,
    session: NewSessionResponse,
) -> NegotiatedAcpCapabilities {
    let modes = session
        .modes
        .map(|state| {
            let current = state.current_mode_id.0;
            state
                .available_modes
                .into_iter()
                .map(|mode| AcpSessionModeDescriptor {
                    current: mode.id.0 == current,
                    native_id: mode.id.0.to_string(),
                    name: mode.name,
                    description: mode.description,
                })
                .collect()
        })
        .unwrap_or_default();
    let config_options = session
        .config_options
        .unwrap_or_default()
        .into_iter()
        .filter_map(project_config_option)
        .collect();
    let capabilities = init.agent_capabilities;
    NegotiatedAcpCapabilities {
        protocol_version: init.protocol_version.to_string(),
        load_session: capabilities.load_session,
        prompt_image: capabilities.prompt_capabilities.image,
        prompt_audio: capabilities.prompt_capabilities.audio,
        prompt_embedded_context: capabilities.prompt_capabilities.embedded_context,
        mcp_http: capabilities.mcp_capabilities.http,
        mcp_sse: capabilities.mcp_capabilities.sse,
        session_list: capabilities.session_capabilities.list.is_some(),
        modes,
        config_options,
    }
}

pub(crate) fn mode_ids(modes: Option<SessionModeState>) -> Vec<String> {
    modes
        .map(|state| {
            state
                .available_modes
                .into_iter()
                .map(|mode| mode.id.0.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn config_option_ids(options: Option<Vec<SessionConfigOption>>) -> Vec<String> {
    options
        .unwrap_or_default()
        .into_iter()
        .map(|option| option.id.0.to_string())
        .collect()
}

fn project_config_option(option: SessionConfigOption) -> Option<AcpSessionConfigOptionDescriptor> {
    let category = option.category.map(|category| match category {
        SessionConfigOptionCategory::Mode => "mode".to_string(),
        SessionConfigOptionCategory::Model => "model".to_string(),
        SessionConfigOptionCategory::ThoughtLevel => "thought_level".to_string(),
        SessionConfigOptionCategory::Other(value) => value,
        _ => "unknown".to_string(),
    });
    let SessionConfigKind::Select(select) = option.kind else {
        return None;
    };
    let choices = match select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .into_iter()
            .map(|choice| AcpSessionConfigChoice {
                native_value: choice.value.0.to_string(),
                name: choice.name,
                description: choice.description,
                group_id: None,
                group_name: None,
            })
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .into_iter()
            .flat_map(|group| {
                let group_id = group.group.0.to_string();
                let group_name = group.name;
                group
                    .options
                    .into_iter()
                    .map(move |choice| AcpSessionConfigChoice {
                        native_value: choice.value.0.to_string(),
                        name: choice.name,
                        description: choice.description,
                        group_id: Some(group_id.clone()),
                        group_name: Some(group_name.clone()),
                    })
            })
            .collect(),
        _ => return None,
    };
    Some(AcpSessionConfigOptionDescriptor {
        native_id: option.id.0.to_string(),
        name: option.name,
        description: option.description,
        category,
        current_value: select.current_value.0.to_string(),
        choices,
    })
}
