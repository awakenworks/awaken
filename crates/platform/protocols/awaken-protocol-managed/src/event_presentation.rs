// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Presentation of known native Agent slots confers no authority;
// @formal reason: all DTO/event serialization remains owned by the API-version projection.
//! Private conversation presentation selection, not a Session state machine.
#[derive(Clone)]
pub struct HideSessionEventDefinitions;
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentDefinitionPresentation {
    #[default]
    Full,
    Manifest,
    Omit,
}
pub(crate) fn manifest(agent: &mut serde_json::Value) {
    // Called only for a typed ordinary Agent slot. Advisor's distinct two-field
    // identity is left untouched by its typed SessionThread projection.
    agent
        .as_object_mut()
        .expect("typed ordinary Agent")
        .retain(|key, _| {
            matches!(
                key.as_str(),
                "id" | "name" | "version" | "type" | "execution_identity"
            )
        });
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::session::SessionAgent;
    use crate::{
        common::api_version::ManagedApiVersion,
        types::{Event, ListEventsResponse, ModelConfig, OutboundKind, PageCursor},
    };
    use serde_json::json;
    fn updated() -> Event {
        Event {
            id: "native-original".into(),
            processed_at: None,
            kind: OutboundKind::SessionUpdated {
                title: Some(None),
                metadata: std::collections::BTreeMap::from([(
                    "agent".into(),
                    "person's metadata".into(),
                )]),
                budget: None,
                agent: Some(SessionAgent {
                    id: "published".into(),
                    kind: "agent",
                    version: 7,
                    name: "本人助理".into(),
                    model: ModelConfig::new("private-model"),
                    description: Some("PRIVATE DESCRIPTION".into()),
                    system: Some("SECRET INSTRUCTIONS".into()),
                    tools: vec![],
                    mcp_servers: vec![],
                    skills: vec![],
                    multiagent: None,
                }),
            },
        }
    }
    #[test]
    fn private_history_pages_and_stream_event_use_one_api_date_owner_and_preserve_completion_identity()
     {
        let event = updated();
        for version in [ManagedApiVersion::Legacy, ManagedApiVersion::Current] {
            for mode in [
                AgentDefinitionPresentation::Manifest,
                AgentDefinitionPresentation::Omit,
            ] {
                let streamed =
                    serde_json::to_value(version.wrap(&event).with_agent_presentation(mode))
                        .unwrap();
                let history = serde_json::to_value(
                    version
                        .wrap(ListEventsResponse {
                            data: vec![event.clone()],
                            next_page: Some("original-cursor".into()),
                        })
                        .with_agent_presentation(mode),
                )
                .unwrap();
                let page = serde_json::to_value(
                    version
                        .wrap(PageCursor::single(vec![event.clone()]))
                        .with_agent_presentation(mode),
                )
                .unwrap();
                assert_eq!(streamed, history["data"][0]);
                assert_eq!(streamed, page["data"][0]);
                assert_eq!(history["next_page"], "original-cursor");
                assert_eq!(streamed["id"], "native-original");
                assert!(streamed["processed_at"].is_null());
                assert!(streamed["title"].is_null());
                assert_eq!(streamed["metadata"]["agent"], "person's metadata");
                assert!(!streamed.to_string().contains("SECRET INSTRUCTIONS"));
                assert!(!streamed.to_string().contains("private-model"));
                if mode == AgentDefinitionPresentation::Omit {
                    assert!(streamed.get("agent").is_none());
                } else {
                    assert_eq!(streamed["agent"]["name"], "本人助理");
                    assert_eq!(
                        streamed["agent"].get("execution_identity").is_some(),
                        version == ManagedApiVersion::Current
                    );
                }
            }
        }
    }
    #[test]
    fn legacy_full_wire_and_tool_payloads_keep_exact_bytes_and_native_processing_enrichment() {
        let event = updated();
        let expected = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::to_string(&ManagedApiVersion::Legacy.wrap(&event)).unwrap(),
            expected
        );
        let mut tool = Event {
            id: "native-tool".into(),
            processed_at: None,
            kind: OutboundKind::AgentCustomToolUse {
                name: "propose_command".into(),
                input: json!({"agent":{"system":"person's actual input"},"citations":["source"]}),
                session_thread_id: None,
            },
        };
        for stamp in [None, Some("2026-11-09T01:02:03Z".into())] {
            tool.processed_at = stamp;
            let expected = serde_json::to_string(&tool).unwrap();
            for mode in [
                AgentDefinitionPresentation::Full,
                AgentDefinitionPresentation::Manifest,
                AgentDefinitionPresentation::Omit,
            ] {
                assert_eq!(
                    serde_json::to_string(
                        &ManagedApiVersion::Legacy
                            .wrap(&tool)
                            .with_agent_presentation(mode)
                    )
                    .unwrap(),
                    expected
                );
            }
        }
        let mut own = json!({"id":"a","type":"agent","version":1,"name":"published","system":"hidden","multiagent":{"agents":[{"system":"hidden child"}]}});
        manifest(&mut own);
        assert_eq!(
            own,
            json!({"id":"a","type":"agent","version":1,"name":"published"})
        );
    }
}
