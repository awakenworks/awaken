// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: The private ingress lowers only deployment-selected creates and restricted owner events.
//! Closed private writes over the original native Session and File handlers.

use std::collections::HashSet;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::types::session::InboundEvent;
use awaken_session_contract::SessionPrivateBinding;
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use serde::Deserialize;

const MAX_BODY: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    org_id: String,
    application_id: String,
    workspace_id: String,
    application_binding_revision: u64,
    agent_id: String,
    agent_version: u64,
    environment_id: String,
}

pub(super) struct Profiles(Vec<Profile>);

impl Profiles {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BODY {
            return Err("private write profile projection exceeds its bound".into());
        }
        let profiles: Vec<Profile> = serde_json::from_slice(bytes)
            .map_err(|_| "invalid private write profile projection")?;
        let mut seen = HashSet::new();
        for profile in &profiles {
            let exact = |value: &str| {
                !value.is_empty()
                    && value.len() <= 256
                    && value.trim() == value
                    && !value.chars().any(char::is_control)
            };
            if ![
                &profile.org_id,
                &profile.application_id,
                &profile.workspace_id,
                &profile.agent_id,
                &profile.environment_id,
            ]
            .into_iter()
            .all(|value| exact(value))
                || profile.agent_version == 0
                || profile.application_binding_revision == 0
                || !seen.insert((
                    &profile.org_id,
                    &profile.application_id,
                    &profile.workspace_id,
                    profile.application_binding_revision,
                ))
            {
                return Err(
                    "private write profiles require distinct exact binding and release coordinates"
                        .into(),
                );
            }
        }
        Ok(Self(profiles))
    }

    pub(super) fn for_binding(&self, binding: &SessionPrivateBinding) -> Option<&Profile> {
        self.0.iter().find(|profile| {
            profile.org_id == binding.org_id()
                && profile.application_id == binding.application_id()
                && profile.workspace_id == binding.workspace_id()
                && profile.application_binding_revision == binding.application_binding_revision()
        })
    }
}

pub(super) enum Write {
    Create,
    Events,
    Upload,
    Stop,
}

impl Write {
    pub(super) fn classify(path: &str) -> Option<(String, Self)> {
        let operation = path.strip_prefix("/v1/private-assistant/")?;
        let path: Vec<_> = operation.split('/').collect();
        let (canonical, kind) = match path.as_slice() {
            ["sessions"] => ("/v1/sessions".to_owned(), Self::Create),
            ["sessions", id, "events"] if !id.is_empty() => {
                (format!("/v1/sessions/{id}/events"), Self::Events)
            }
            ["sessions", id, "stop"] if !id.is_empty() => {
                (format!("/v1/sessions/{id}/events"), Self::Stop)
            }
            ["files"] => ("/v1/files".to_owned(), Self::Upload),
            _ => return None,
        };
        Some((canonical, kind))
    }

    pub(super) async fn body(
        self,
        headers: &mut HeaderMap,
        body: Body,
        profile: Option<&Profile>,
    ) -> Result<Body, StatusCode> {
        headers.remove("anthropic-user-profile-id");
        if matches!(self, Self::Upload) {
            return Ok(body);
        }
        let limit = if matches!(self, Self::Stop) {
            0
        } else {
            MAX_BODY
        };
        let bytes = to_bytes(body, limit)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        let json = match self {
            Self::Stop => Body::from(r#"{"events":[{"type":"user.interrupt"}]}"#),
            Self::Create => {
                let profile = profile.ok_or(StatusCode::NOT_FOUND)?;
                let requested: Create =
                    serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
                Body::from(serde_json::json!({
                    "agent": {"type": "agent", "id": profile.agent_id, "version": profile.agent_version},
                    "environment_id": profile.environment_id, "title": requested.title,
                }).to_string())
            }
            Self::Events => {
                let requested: Events =
                    serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
                if requested.events.is_empty()
                    || requested.events.len() > 32
                    || !requested.events.iter().all(personal_event)
                {
                    return Err(StatusCode::BAD_REQUEST);
                }
                Body::from(bytes)
            }
            Self::Upload => unreachable!("multipart is passed directly to the native owner"),
        };
        headers.remove(header::CONTENT_LENGTH);
        headers.remove(header::CONTENT_ENCODING);
        headers.remove(header::TRANSFER_ENCODING);
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Ok(json)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    events: Vec<InboundEvent>,
}

fn personal_event(event: &InboundEvent) -> bool {
    let text = |content: &[ContentBlock]| {
        content
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { .. }))
    };
    match event {
        InboundEvent::UserMessage { content } => !content.is_empty() && text(content),
        InboundEvent::UserToolResult { content, .. }
        | InboundEvent::UserCustomToolResult { content, .. } => content.as_deref().is_none_or(text),
        InboundEvent::UserToolConfirmation { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"{
        "org_id":"company-a","application_id":"desk","workspace_id":"private-a",
        "application_binding_revision":3,"agent_id":"assistant-a","agent_version":7,
        "environment_id":"environment-a"
    }"#;

    #[tokio::test]
    async fn create_uses_only_the_exact_current_binding_and_release() {
        let profiles = Profiles::parse(format!("[{PROFILE}]").as_bytes()).unwrap();
        assert!(Profiles::parse(format!("[{PROFILE},{PROFILE}]").as_bytes()).is_err());
        let binding = |org: &str, revision| {
            SessionPrivateBinding::new(
                org.into(),
                "desk".into(),
                "alice".into(),
                "private-a".into(),
                revision,
            )
            .unwrap()
        };
        assert!(profiles.for_binding(&binding("company-b", 3)).is_none());
        assert!(profiles.for_binding(&binding("company-a", 4)).is_none());
        let profile = profiles.for_binding(&binding("company-a", 3));
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-user-profile-id",
            HeaderValue::from_static("foreign"),
        );
        let body = Write::Create
            .body(
                &mut headers,
                Body::from(r#"{"title":"my session"}"#),
                profile,
            )
            .await
            .unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(body, MAX_BODY).await.unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "title":"my session",
                "agent":{"type":"agent","id":"assistant-a","version":7},
                "environment_id":"environment-a"
            })
        );
        assert!(!headers.contains_key("anthropic-user-profile-id"));
        assert!(matches!(
            Write::Create
                .body(
                    &mut headers,
                    Body::from(r#"{"environment_id":"foreign"}"#),
                    profile
                )
                .await,
            Err(StatusCode::BAD_REQUEST)
        ));
    }

    #[tokio::test]
    async fn events_refuse_interrupt_and_unverified_content() {
        let cases = [
            (
                r#"{"events":[{"type":"user.message","content":[{"type":"text","text":"hello"}]}]}"#,
                true,
            ),
            (r#"{"events":[{"type":"user.interrupt"}]}"#, false),
            (
                r#"{"events":[{"type":"user.message","content":[{"type":"file","file_id":"foreign"}]}]}"#,
                false,
            ),
            (r#"{"events":[]}"#, false),
        ];
        for (input, accepted) in cases {
            let result = Write::Events
                .body(&mut HeaderMap::new(), Body::from(input), None)
                .await;
            assert_eq!(result.is_ok(), accepted, "{input}");
        }
    }
}
