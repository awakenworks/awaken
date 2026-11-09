//! Explicit API-date projection over the existing Managed DTOs, without state.
use axum::Json;
use axum::extract::FromRequestParts;
use axum::http::{HeaderMap, StatusCode, request::Parts};
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::event_presentation::{AgentDefinitionPresentation, manifest};
use crate::types::agent::{Agent, ServiceAccountExecutionIdentity};
use crate::types::session::{
    SessionAgent, SessionMultiagentRosterEntry, SessionThreadAgent, SessionThreadAgentValue,
};
use crate::types::{
    ErrorResponse, Event, ListEventsResponse, OutboundKind, PageCursor, SendEventsResponse,
    Session, SessionThread,
};

/// Encode an already-authorized native Agent page with the exact original API
/// date contract. This presentation helper authenticates and grants nothing.
pub fn managed_agent_page_response(
    headers: &HeaderMap,
    page: PageCursor<Agent>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    match ManagedApiVersion::from_headers(headers) {
        Ok(version) => Json(version.wrap(page)).into_response(),
        Err(error) => error.into_response(),
    }
}
pub(crate) type VersionError = (StatusCode, Json<ErrorResponse>);
pub(crate) const CURRENT_API_VERSION: &str = "2026-10-21";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedApiVersion {
    Legacy,
    Current,
}

impl ManagedApiVersion {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Result<Self, VersionError> {
        let mut values = headers.get_all("anthropic-version").iter();
        match (values.next(), values.next()) {
            (None, None) => Ok(Self::Legacy),
            (Some(value), None)
                if value.as_bytes() == super::headers::ANTHROPIC_API_VERSION.as_bytes() =>
            {
                Ok(Self::Legacy)
            }
            (Some(value), None) if value.as_bytes() == CURRENT_API_VERSION.as_bytes() => {
                Ok(Self::Current)
            }
            _ => Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "anthropic-version requires one supported value: 2023-06-01 or 2026-10-21",
                )),
            )),
        }
    }

    pub(crate) fn wrap<T: ApiProjection>(self, value: T) -> ApiWire<T> {
        ApiWire {
            value,
            version: self,
            presentation: AgentDefinitionPresentation::Full,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ManagedApiVersion {
    type Rejection = VersionError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Self::from_headers(&parts.headers)
    }
}

/// The legacy serializer remains exact. Only typed Managed DTOs implement this
/// projection; this is not a middleware rewriting arbitrary JSON responses.
pub(crate) struct ApiWire<T> {
    value: T,
    version: ManagedApiVersion,
    presentation: AgentDefinitionPresentation,
}
impl<T: ApiProjection> ApiWire<T> {
    pub(crate) fn with_agent_presentation(
        mut self,
        presentation: AgentDefinitionPresentation,
    ) -> Self {
        self.presentation = presentation;
        self
    }
}
impl<T: ApiProjection> Serialize for ApiWire<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let project = self.value.needs_definition_projection(self.presentation);
        if self.version == ManagedApiVersion::Legacy && !project {
            return self.value.serialize(serializer);
        }
        let mut value = serde_json::to_value(&self.value).map_err(serde::ser::Error::custom)?;
        if self.version == ManagedApiVersion::Current {
            self.value.project_identity(&mut value);
        }
        if project {
            self.value
                .project_definitions(&mut value, self.presentation);
        }
        value.serialize(serializer)
    }
}

pub(crate) trait ApiProjection: Serialize {
    fn project_identity(&self, value: &mut Value);
    fn needs_definition_projection(&self, _presentation: AgentDefinitionPresentation) -> bool {
        false
    }
    fn project_definitions(&self, _value: &mut Value, _presentation: AgentDefinitionPresentation) {}
}
impl<T: ApiProjection> ApiProjection for &T {
    fn needs_definition_projection(&self, presentation: AgentDefinitionPresentation) -> bool {
        T::needs_definition_projection(*self, presentation)
    }
    fn project_definitions(&self, value: &mut Value, presentation: AgentDefinitionPresentation) {
        T::project_definitions(*self, value, presentation)
    }
    fn project_identity(&self, value: &mut Value) {
        (*self).project_identity(value);
    }
}

fn service_identity(value: &mut Value) {
    value["execution_identity"] =
        serde_json::to_value(ServiceAccountExecutionIdentity::ServiceAccount {})
            .expect("identity serializes");
}
impl ApiProjection for Agent {
    fn project_identity(&self, value: &mut Value) {
        service_identity(value);
    }
}
impl ApiProjection for SessionThreadAgent {
    fn project_identity(&self, value: &mut Value) {
        service_identity(value);
    }
}
impl ApiProjection for SessionAgent {
    fn project_identity(&self, value: &mut Value) {
        service_identity(value);
        if let Some(roster) = &self.multiagent {
            for (entry, row) in roster.agents.iter().zip(
                value["multiagent"]["agents"]
                    .as_array_mut()
                    .expect("typed roster"),
            ) {
                if let SessionMultiagentRosterEntry::Agent(agent) = entry {
                    agent.project_identity(row);
                }
            }
        }
    }
}
impl ApiProjection for Session {
    fn needs_definition_projection(&self, presentation: AgentDefinitionPresentation) -> bool {
        presentation == AgentDefinitionPresentation::Manifest
    }
    fn project_definitions(&self, value: &mut Value, _presentation: AgentDefinitionPresentation) {
        manifest(&mut value["agent"]);
    }
    fn project_identity(&self, value: &mut Value) {
        self.agent.project_identity(&mut value["agent"]);
    }
}
impl ApiProjection for SessionThread {
    fn needs_definition_projection(&self, presentation: AgentDefinitionPresentation) -> bool {
        presentation == AgentDefinitionPresentation::Manifest
            && matches!(self.agent, SessionThreadAgentValue::Agent(_))
    }
    fn project_definitions(&self, value: &mut Value, presentation: AgentDefinitionPresentation) {
        if self.needs_definition_projection(presentation) {
            manifest(&mut value["agent"]);
        }
    }
    fn project_identity(&self, value: &mut Value) {
        if let SessionThreadAgentValue::Agent(agent) = &self.agent {
            agent.project_identity(&mut value["agent"]);
        }
    }
}
impl ApiProjection for Event {
    fn needs_definition_projection(&self, presentation: AgentDefinitionPresentation) -> bool {
        presentation != AgentDefinitionPresentation::Full
            && matches!(
                self.kind,
                OutboundKind::SessionUpdated { agent: Some(_), .. }
            )
    }
    fn project_definitions(&self, value: &mut Value, presentation: AgentDefinitionPresentation) {
        if !self.needs_definition_projection(presentation) {
            return;
        }
        match presentation {
            AgentDefinitionPresentation::Manifest => manifest(&mut value["agent"]),
            AgentDefinitionPresentation::Omit => {
                value.as_object_mut().expect("typed Event").remove("agent");
            }
            AgentDefinitionPresentation::Full => {}
        }
    }
    fn project_identity(&self, value: &mut Value) {
        if let OutboundKind::SessionUpdated {
            agent: Some(agent), ..
        } = &self.kind
        {
            agent.project_identity(&mut value["agent"]);
        }
    }
}

pub(crate) fn project_definition_rows<T: ApiProjection>(
    rows: &[T],
    value: &mut Value,
    presentation: AgentDefinitionPresentation,
) {
    for (row, projected) in rows
        .iter()
        .zip(value["data"].as_array_mut().expect("typed page"))
    {
        row.project_definitions(projected, presentation);
    }
}
pub(crate) fn project_rows<T: ApiProjection>(rows: &[T], value: &mut Value) {
    for (row, projected) in rows
        .iter()
        .zip(value["data"].as_array_mut().expect("typed page"))
    {
        row.project_identity(projected);
    }
}
impl<T: ApiProjection> ApiProjection for PageCursor<T> {
    fn needs_definition_projection(&self, presentation: AgentDefinitionPresentation) -> bool {
        self.data
            .iter()
            .any(|row| row.needs_definition_projection(presentation))
    }
    fn project_definitions(&self, value: &mut Value, presentation: AgentDefinitionPresentation) {
        project_definition_rows(&self.data, value, presentation);
    }
    fn project_identity(&self, value: &mut Value) {
        project_rows(&self.data, value);
    }
}
impl ApiProjection for ListEventsResponse {
    fn needs_definition_projection(&self, presentation: AgentDefinitionPresentation) -> bool {
        self.data
            .iter()
            .any(|row| row.needs_definition_projection(presentation))
    }
    fn project_definitions(&self, value: &mut Value, presentation: AgentDefinitionPresentation) {
        project_definition_rows(&self.data, value, presentation);
    }
    fn project_identity(&self, value: &mut Value) {
        project_rows(&self.data, value);
    }
}
impl ApiProjection for SendEventsResponse {
    fn needs_definition_projection(&self, presentation: AgentDefinitionPresentation) -> bool {
        self.data
            .iter()
            .any(|row| row.needs_definition_projection(presentation))
    }
    fn project_definitions(&self, value: &mut Value, presentation: AgentDefinitionPresentation) {
        project_definition_rows(&self.data, value, presentation);
    }
    fn project_identity(&self, value: &mut Value) {
        project_rows(&self.data, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frozen_agent() -> SessionAgent {
        let mut agent = SessionAgent {
            id: "root".into(),
            kind: "agent",
            version: 1,
            model: crate::types::ModelConfig::new("model"),
            name: "Root".into(),
            description: None,
            system: None,
            tools: vec![],
            mcp_servers: vec![],
            skills: vec![],
            multiagent: None,
        };
        agent.multiagent = Some(crate::types::session::SessionMultiagentCoordinator {
            kind: "coordinator",
            agents: vec![
                SessionMultiagentRosterEntry::Agent(SessionThreadAgent::from(&agent)),
                SessionMultiagentRosterEntry::Advisor(crate::types::agent::AdvisorRosterEntry {
                    kind: crate::types::agent::AdvisorRosterEntryKind::Advisor,
                    model: "model".into(),
                }),
            ],
        });
        agent
    }

    #[test]
    fn api_date_projects_frozen_identity_without_padding_advisors_or_changing_legacy() {
        // Cause/effect graph: C1 legacy/current date; C2 root/ordinary child/
        // advisor snapshot; C3 single/page/event projection. V1 legacy -> exact
        // bytes with no extra key; V2 current -> service-account identity on
        // executable snapshots only; V3 advisor -> exact original two fields;
        // V4 all container forms -> same frozen identity, no store mutation.
        let agent = frozen_agent();
        let event = Event {
            id: "e".into(),
            processed_at: None,
            kind: OutboundKind::SessionUpdated {
                title: None,
                metadata: Default::default(),
                agent: Some(agent.clone()),
                budget: None,
            },
        };
        assert_eq!(
            serde_json::to_string(&ManagedApiVersion::Legacy.wrap(&event)).unwrap(),
            serde_json::to_string(&event).unwrap()
        );
        let current = serde_json::to_value(ManagedApiVersion::Current.wrap(&event)).unwrap();
        assert_eq!(
            current["agent"]["execution_identity"],
            serde_json::json!({"type":"service_account"})
        );
        assert_eq!(
            current["agent"]["multiagent"]["agents"][0]["execution_identity"]["type"],
            "service_account"
        );
        assert_eq!(
            current["agent"]["multiagent"]["agents"][1],
            serde_json::json!({"type":"advisor", "model":"model"})
        );
        let page = PageCursor::single(vec![event.clone()]);
        assert_eq!(
            serde_json::to_value(ManagedApiVersion::Current.wrap(page)).unwrap()["data"][0],
            current
        );
        let receipt = SendEventsResponse {
            data: vec![event.clone()],
        };
        assert_eq!(
            serde_json::to_value(ManagedApiVersion::Current.wrap(receipt)).unwrap()["data"][0],
            current
        );
        let history = ListEventsResponse {
            data: vec![event.clone()],
            next_page: None,
        };
        assert_eq!(
            serde_json::to_value(ManagedApiVersion::Current.wrap(history)).unwrap()["data"][0],
            current
        );
        let no_snapshot = Event {
            id: "e2".into(),
            processed_at: None,
            kind: OutboundKind::SessionUpdated {
                title: None,
                metadata: Default::default(),
                agent: None,
                budget: None,
            },
        };
        assert_eq!(
            serde_json::to_value(ManagedApiVersion::Current.wrap(&no_snapshot)).unwrap(),
            serde_json::to_value(&no_snapshot).unwrap()
        );
        assert!(
            serde_json::to_value(&event).unwrap()["agent"]
                .get("execution_identity")
                .is_none()
        );
    }

    #[test]
    fn api_date_requires_one_exact_reviewed_version() {
        // V1 absent/legacy -> exact legacy DTO; V2 current -> identity-aware
        // projection; V3 unknown/empty/comma/repeated/non-UTF8 -> 400. Header
        // telemetry and beta select no API date and cannot weaken this parser.
        assert_eq!(
            ManagedApiVersion::from_headers(&HeaderMap::new()).unwrap(),
            ManagedApiVersion::Legacy
        );
        for (value, expected) in [
            ("2023-06-01", Some(ManagedApiVersion::Legacy)),
            ("2026-10-21", Some(ManagedApiVersion::Current)),
            ("", None),
            ("2026-10-22", None),
            ("2026-10-21,2026-10-21", None),
        ] {
            let mut headers = HeaderMap::new();
            headers.append("anthropic-version", value.parse().unwrap());
            assert_eq!(ManagedApiVersion::from_headers(&headers).ok(), expected);
            headers.append("anthropic-version", value.parse().unwrap());
            assert_eq!(
                ManagedApiVersion::from_headers(&headers).unwrap_err().0,
                StatusCode::BAD_REQUEST
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-version",
            axum::http::HeaderValue::from_bytes(&[0x80]).unwrap(),
        );
        assert_eq!(
            ManagedApiVersion::from_headers(&headers).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }
}
