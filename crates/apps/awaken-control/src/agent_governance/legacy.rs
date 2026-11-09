// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: The legacy Config/Managed wire is behind the same current fine
// @formal reason: native admission, not a Workspace role or a browser label.
//! Enterprise definition ingress. Open and fixed local operator contracts remain separate.
use super::{
    AgentGovernanceStores,
    authority::{self, Failure},
    routes::{Admission, Ingress, RequestContext},
};
use awaken_agent_config::{ScopeId, catalog_snapshot::AgentCatalogSnapshot};
use awaken_config_service::ConfigPlane;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{FromRequestParts, Query, RawPathParams, Request, State},
    http::Method,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::Arc;

use crate::authz::AdmittedDefinition;
#[derive(Clone)]
struct DefinitionIngress {
    admission: Ingress,
    plane: ConfigPlane,
    clock: Arc<dyn awaken_time::WallClock>,
}
pub(crate) fn protect(
    router: Router,
    stores: AgentGovernanceStores,
    iam: Option<Arc<crate::RemoteManagementAuthz>>,
    plane: ConfigPlane,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Router {
    if iam.is_none() || stores.config.is_none() {
        return router;
    }
    router.layer(axum::middleware::from_fn_with_state(
        DefinitionIngress {
            admission: Ingress { stores, iam },
            plane,
            clock,
        },
        guard,
    ))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    ConfigCollection,
    ManagedCollection,
    Agent(&'static str),
    Publication,
    Closed,
}
fn classify(method: &Method, path: &str) -> Option<Operation> {
    let parts = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    Some(match (method, parts.as_slice()) {
        (&Method::GET, ["v1", "config", "agents"]) => Operation::ConfigCollection,
        (&Method::GET, ["v1", "agents"]) => Operation::ManagedCollection,
        (&Method::GET, ["v1", "config", "agents", _])
        | (&Method::GET, ["v1", "config", "agents", _, "versions" | "resources"])
        | (&Method::GET, ["v1", "agents", _])
        | (&Method::GET, ["v1", "agents", _, "versions"]) => {
            Operation::Agent(authority::DEFINITION_READ)
        }
        (&Method::PUT, ["v1", "config", "agents", _]) => Operation::Agent("agent.definition.write"),
        (&Method::POST, ["v1", "config", "agents", _, "validate"]) => {
            Operation::Agent("agent.definition.validate")
        }
        (&Method::POST, ["v1", "config", "agents", _, "archive"]) => {
            Operation::Agent("agent.definition.archive")
        }
        (&Method::GET, ["v1", "config", "publications", _])
        | (&Method::GET, ["v1", "config", "publications", _, "export"]) => Operation::Publication,
        // These legacy calls cannot establish durable author provenance or a
        // distinct responsible reviewer. Fine write/use is not publish approval.
        (_, ["v1", "config", "agents", _, "publish" | "ensure"])
        | (_, ["v1", "config", "agent-previews", ..])
        | (&Method::POST, ["v1", "agents"])
        | (&Method::POST, ["v1", "agents", _])
        | (&Method::POST, ["v1", "agents", _, "archive"])
        | (_, ["v1", "awaken", "agents", ..]) => Operation::Closed,
        // Newly mounted definition paths inherit closed admission until their
        // exact native action/baseline contract is reviewed here.
        (_, ["v1", "config", "agents", ..])
        | (_, ["v1", "config", "publications", ..])
        | (_, ["v1", "agents", ..]) => Operation::Closed,
        _ => return None,
    })
}
async fn guard(State(state): State<DefinitionIngress>, request: Request, next: Next) -> Response {
    let Some(operation) = classify(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };
    let mut response = match admitted(state, request, next, operation).await {
        Ok(response) => response,
        Err(failure) => super::routes::managed_failure(failure),
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
/// Read-only native consumers keep exact definition admission; every write and
/// owner collection still requires the direct current person contract.
enum DefinitionAccess {
    Person(Admission),
    Reader(super::managed_definition_reads::DefinitionAdmission),
}
impl DefinitionAccess {
    fn principal(&self) -> &awaken_iam_contract::PrincipalRef {
        match self {
            Self::Person(value) => &value.person.principal,
            Self::Reader(value) => &value.principal,
        }
    }
    async fn allows(&self, id: &str, demand: &'static str) -> Result<bool, Failure> {
        match self {
            Self::Person(value) => value.allows(id, demand).await,
            Self::Reader(value) => value.allows(id, demand).await,
        }
    }
    async fn end(&self, context: &RequestContext) -> Result<(), Failure> {
        match self {
            Self::Person(value) => value.end(context).await,
            Self::Reader(value) => value.end(context).await,
        }
    }
}
async fn admitted(
    state: DefinitionIngress,
    request: Request,
    next: Next,
    operation: Operation,
) -> Result<Response, Failure> {
    let context = RequestContext::from_request(&request)?;
    if operation == Operation::Closed {
        // Authenticate as a person even for a closed management operation.
        Admission::begin(&state.admission, &context).await?;
        return Err(Failure::Denied);
    }
    let admission = if matches!(
        operation,
        Operation::Agent(authority::DEFINITION_READ) | Operation::Publication
    ) {
        DefinitionAccess::Reader(
            super::managed_definition_reads::DefinitionAdmission::begin(
                state.admission.iam.clone().ok_or(Failure::NotConfigured)?,
                state.admission.stores.config.as_ref(),
                &context,
            )
            .await?,
        )
    } else {
        DefinitionAccess::Person(Admission::begin(&state.admission, &context).await?)
    };
    let scope = ScopeId::from(context.workspace.as_str());
    let initial = state
        .admission
        .stores
        .catalog
        .agent_catalog_epoch(&scope)
        .await
        .map_err(|_| Failure::Unavailable)?;
    if matches!(
        operation,
        Operation::ConfigCollection | Operation::ManagedCollection
    ) {
        let DefinitionAccess::Person(person) = &admission else {
            return Err(Failure::Denied);
        };
        let params = if operation == Operation::ManagedCollection {
            Some(
                Query::<awaken_protocol_managed::types::agent::AgentListParams>::try_from_uri(
                    request.uri(),
                )
                .map_err(|_| Failure::Invalid)?
                .0,
            )
        } else {
            None
        };
        let snapshot = state
            .admission
            .stores
            .catalog
            .agent_catalog_snapshot(&scope, &person.person.owner, 4096)
            .await
            .map_err(|_| Failure::Unavailable)?;
        let response = collection(
            &state,
            person,
            params,
            request.headers().clone(),
            snapshot,
            operation,
        )
        .await?;
        admission.end(&context).await?;
        if state
            .admission
            .stores
            .catalog
            .agent_catalog_epoch(&scope)
            .await
            .map_err(|_| Failure::Unavailable)?
            != initial
        {
            return Err(Failure::Unavailable);
        }
        return Ok(response);
    }
    let is_put = request.method() == Method::PUT && !request.uri().path().ends_with("/resources");
    let (mut parts, body) = request.into_parts();
    let body = if is_put {
        let bytes = to_bytes(body, 2 * 1024 * 1024)
            .await
            .map_err(|_| Failure::Invalid)?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)?;
        value
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .ok_or(Failure::Invalid)?;
        Body::from(bytes)
    } else {
        body
    };
    let parameters = RawPathParams::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| Failure::Invalid)?;
    let find = |key| {
        parameters
            .iter()
            .find_map(|(name, value)| (name == key).then(|| value.to_owned()))
    };
    let (id, demand) = match operation {
        Operation::Agent(demand) => (
            find("id")
                .or_else(|| find("agent_id"))
                .ok_or(Failure::Invalid)?,
            demand,
        ),
        Operation::Publication => {
            let fingerprint = find("fingerprint").ok_or(Failure::Invalid)?;
            let publication = state
                .plane
                .publication(&scope, &fingerprint)
                .await
                .map_err(|_| Failure::Unavailable)?
                .ok_or(Failure::Denied)?;
            (publication.agent_id, authority::DEFINITION_READ)
        }
        _ => return Err(Failure::Invalid),
    };
    if !admission.allows(&id, demand).await? {
        return Err(Failure::Denied);
    }
    // Native read principals never become a human write actor. This branch
    // admitted only exact reads or a separately verified human operation.
    parts.extensions.insert(AdmittedDefinition);
    parts.extensions.insert(admission.principal().clone());
    parts
        .extensions
        .insert(crate::authz::AuthedPrincipal(admission.principal().clone()));
    parts
        .extensions
        .insert(awaken_runtime_contract::runs::scope::WorkspaceScope(
            context.workspace.clone(),
        ));
    let writing = parts.method != Method::GET;
    let response = next.run(Request::from_parts(parts, body)).await;
    // A write may already have committed. A failed ending observation therefore
    // means unknown, never a claim that a 401/403 proves no mutation occurred.
    admission
        .end(&context)
        .await
        .map_err(|e| if writing { Failure::Unavailable } else { e })?;
    if !writing
        && state
            .admission
            .stores
            .catalog
            .agent_catalog_epoch(&scope)
            .await
            .map_err(|_| Failure::Unavailable)?
            != initial
    {
        return Err(Failure::Unavailable);
    }
    Ok(response)
}
async fn collection(
    state: &DefinitionIngress,
    admission: &Admission,
    params: Option<awaken_protocol_managed::types::agent::AgentListParams>,
    headers: axum::http::HeaderMap,
    snapshot: AgentCatalogSnapshot,
    operation: Operation,
) -> Result<Response, Failure> {
    let mut entries = Vec::new();
    for entry in snapshot.entries {
        if admission
            .allows(&entry.head.config.id, authority::DEFINITION_READ)
            .await?
        {
            entries.push(entry);
        }
    }
    if operation == Operation::ConfigCollection {
        let rows = entries
            .into_iter()
            .map(|entry| {
                let published = entry
                    .published
                    .as_ref()
                    .is_some_and(|p| p.revision == entry.head.revision);
                crate::managed_agent::managed_from_agent_config(&entry.head.config, published)
                    .map_err(|_| Failure::Unavailable)
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Json(json!({"data":rows})).into_response());
    }
    let params = params.ok_or(Failure::Invalid)?;
    let mut agents = Vec::new();
    for entry in entries {
        let agent = crate::managed_agents::project_catalog_entry(entry, state.clock.as_ref())
            .map_err(|_| Failure::Unavailable)?;
        if !params.include_archived && agent.archived_at.is_some() {
            continue;
        }
        if params
            .created_at_gte
            .as_deref()
            .is_some_and(|lower| agent.created_at.as_str() < lower)
            || params
                .created_at_lte
                .as_deref()
                .is_some_and(|upper| agent.created_at.as_str() > upper)
        {
            continue;
        }
        agents.push(agent);
    }
    Ok(awaken_protocol_managed::managed_agent_page_response(
        &headers,
        awaken_protocol_managed::types::paginate(agents, &params.page_query(), |agent| {
            agent.id.as_str()
        }),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_lifecycle_matrix_never_treats_immediate_publish_or_unbound_preview_as_drafts() {
        for path in [
            "/v1/agents",
            "/v1/agents/a",
            "/v1/config/agents/a/publish",
            "/v1/config/agents/__admin_assistant/ensure",
            "/v1/config/agent-previews/preview-a",
            "/v1/awaken/agents/execution-evidence",
        ] {
            assert_eq!(
                classify(&Method::POST, path),
                Some(Operation::Closed),
                "{path}"
            );
        }
        assert_eq!(
            classify(&Method::PUT, "/v1/config/agents/a"),
            Some(Operation::Agent("agent.definition.write"))
        );
        assert_eq!(
            classify(&Method::GET, "/v1/config/agents/a/versions"),
            Some(Operation::Agent(authority::DEFINITION_READ))
        );
        assert_eq!(classify(&Method::POST, "/v1/sessions/a/events"), None);
    }
}
