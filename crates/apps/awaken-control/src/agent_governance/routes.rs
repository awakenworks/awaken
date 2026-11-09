// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Current direct Account, trusted Workspace and whole-page native
// @formal reason: fences precede bounded whitelist projection; no UI role grants access.
use super::{
    AgentGovernanceStores,
    authority::{self, Failure, Person},
};
use crate::{
    RemoteManagementAuthz,
    authz::{authorization_bearer_token, off_event_loop},
};
use awaken_agent_config::personal_selection::{AgentSelectionError, AgentSelectionIntent};
use awaken_agent_config::{ScopeId, catalog_snapshot::AgentCatalogSnapshot};
use awaken_authz_enforce::RequestTenancy;
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub(super) struct Ingress {
    pub(super) stores: AgentGovernanceStores,
    pub(super) iam: Option<Arc<RemoteManagementAuthz>>,
}

pub(crate) fn router(
    stores: AgentGovernanceStores,
    iam: Option<Arc<RemoteManagementAuthz>>,
) -> Router {
    Router::new()
        .route("/v1/agent-directory", get(directory))
        .route("/v1/me/agent-selections", get(super::selection_reads::list))
        .route("/v1/me/agent-selections/{id}", axum::routing::put(select))
        .route("/v1/me/agent-selections/{id}/outcomes/{key}", get(outcome))
        .with_state(Ingress { stores, iam })
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "current-account-required"),
            Self::Denied => (StatusCode::FORBIDDEN, "not-authorized"),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "current-qualification-unavailable",
            ),
            Self::NotConfigured => (
                StatusCode::SERVICE_UNAVAILABLE,
                "agent-governance-not-configured",
            ),
            Self::Invalid => (StatusCode::BAD_REQUEST, "invalid-agent-request"),
        };
        (status, Json(serde_json::json!({"error":code}))).into_response()
    }
}

/// Legacy Managed paths retain their published structured error envelope. The
/// new directory wire keeps its own finite error codes above.
pub(super) fn managed_failure(failure: Failure) -> Response {
    let (status, kind, message) = match failure {
        Failure::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "a current native Account is required",
        ),
        Failure::Denied => (
            StatusCode::FORBIDDEN,
            "permission_error",
            "the native Agent operation is not authorized",
        ),
        Failure::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "current native Agent qualification is unavailable",
        ),
        Failure::NotConfigured => (
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "native Agent governance is not configured",
        ),
        Failure::Invalid => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid Agent request or Workspace selector",
        ),
    };
    (
        status,
        Json(awaken_protocol_managed::types::ErrorResponse::new(
            kind, message,
        )),
    )
        .into_response()
}
pub(super) struct RequestContext {
    pub(super) workspace: String,
    pub(super) token: awaken_agent_contract::RedactedString,
    pub(super) purpose:
        Option<Arc<dyn awaken_tenancy::workspace_purpose::OrdinaryWorkspaceAdmission>>,
}
impl RequestContext {
    pub(super) fn from_request(request: &Request) -> Result<Self, Failure> {
        let trusted = request
            .extensions()
            .get::<RequestTenancy>()
            .map(|s| s.workspace_id.as_str())
            .or_else(|| {
                request
                    .extensions()
                    .get::<awaken_runtime_contract::runs::scope::WorkspaceScope>()
                    .map(|s| s.0.as_str())
            })
            .ok_or(Failure::Denied)?;
        let workspace = awaken_authz_enforce::request_workspace_scope(request, None)
            .map_err(|error| match error {
                awaken_authz_enforce::WorkspaceSelectionError::InvalidSelector => Failure::Invalid,
                awaken_authz_enforce::WorkspaceSelectionError::ConflictingSelector => {
                    Failure::Denied
                }
            })?
            .filter(|s| s == trusted && authority::exact(s, 300))
            .ok_or(Failure::Denied)?;
        if request
            .extensions()
            .get::<awaken_tenancy::ExecutionWorkspace>()
            .is_some_and(|s| s.0 != workspace)
        {
            return Err(Failure::Denied);
        }
        let token = awaken_agent_contract::RedactedString::new(
            authorization_bearer_token(request.headers()).ok_or(Failure::Unauthorized)?,
        );
        let purpose = request
            .extensions()
            .get::<Arc<dyn awaken_tenancy::workspace_purpose::OrdinaryWorkspaceAdmission>>()
            .cloned();
        Ok(Self {
            workspace,
            token,
            purpose,
        })
    }
}
pub(super) struct Admission {
    iam: Arc<RemoteManagementAuthz>,
    pub(super) person: Person,
    token: awaken_agent_contract::RedactedString,
    workspace: String,
    fence: String,
    product: awaken_iam_contract::ProductId,
}
impl Admission {
    pub(super) async fn begin(
        ingress: &Ingress,
        context: &RequestContext,
    ) -> Result<Self, Failure> {
        Self::begin_with(
            ingress.iam.clone().ok_or(Failure::NotConfigured)?,
            ingress.stores.config.as_ref(),
            context,
        )
        .await
    }
    pub(super) async fn begin_with(
        iam: Arc<RemoteManagementAuthz>,
        config: Option<&super::AgentGovernanceConfig>,
        context: &RequestContext,
    ) -> Result<Self, Failure> {
        let product = config.ok_or(Failure::NotConfigured)?.product_id.clone();
        let workspace = context.workspace.clone();
        let token = context.token.clone();
        let current = iam.clone();
        let credential = token.clone();
        let person =
            off_event_loop::run(move || authority::person(&current, credential.expose_secret()))
                .await
                .map_err(|_| Failure::Unavailable)??;
        ordinary(context, &workspace).await?;
        let current = iam.clone();
        let subject = person.clone();
        let target = workspace.clone();
        let fence = off_event_loop::run(move || authority::observe(&current, &subject, &target))
            .await
            .map_err(|_| Failure::Unavailable)??;
        Ok(Self {
            iam,
            person,
            token,
            workspace,
            fence,
            product,
        })
    }
    pub(super) async fn allows(&self, id: &str, demand: &'static str) -> Result<bool, Failure> {
        let product = self.product.clone();
        let (iam, person, workspace, id, fence) = (
            self.iam.clone(),
            self.person.clone(),
            self.workspace.clone(),
            id.to_owned(),
            self.fence.clone(),
        );
        off_event_loop::run(move || {
            authority::allows(&iam, &product, &person, &workspace, &id, demand, &fence)
        })
        .await
        .map_err(|_| Failure::Unavailable)?
    }
    pub(super) async fn end(&self, context: &RequestContext) -> Result<(), Failure> {
        ordinary(context, &self.workspace).await?;
        let (iam, token, expected) = (self.iam.clone(), self.token.clone(), self.person.clone());
        let current = off_event_loop::run(move || authority::person(&iam, token.expose_secret()))
            .await
            .map_err(|_| Failure::Unavailable)??;
        if current != expected {
            return Err(Failure::Unauthorized);
        }
        let (iam, workspace) = (self.iam.clone(), self.workspace.clone());
        let final_fence =
            off_event_loop::run(move || authority::observe(&iam, &current, &workspace))
                .await
                .map_err(|_| Failure::Unavailable)??;
        if final_fence != self.fence {
            return Err(Failure::Unavailable);
        }
        Ok(())
    }
}
pub(super) async fn ordinary(context: &RequestContext, workspace: &str) -> Result<(), Failure> {
    if context.workspace != workspace {
        return Err(Failure::Denied);
    }
    if let Some(port) = context.purpose.as_ref() {
        match port.is_ordinary_workspace(workspace).await {
            Ok(true) => {}
            Ok(false) => return Err(Failure::Denied),
            Err(_) => return Err(Failure::Unavailable),
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryQuery {
    limit: Option<usize>,
    after: Option<String>,
    selected: Option<bool>,
}
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
enum Selection {
    Selected { generation: u64 },
    NotSelected { generation: u64 },
    Unknown,
}
#[derive(Serialize)]
struct Manifest {
    id: String,
    name: String,
    version: u64,
    #[serde(rename = "type")]
    kind: &'static str,
    selection: Selection,
}
#[derive(Serialize)]
struct Directory {
    data: Vec<Manifest>,
    next_page: Option<String>,
    visibility: &'static str,
}

async fn directory(
    State(ingress): State<Ingress>,
    Query(query): Query<DirectoryQuery>,
    request: Request,
) -> Result<Json<Directory>, Failure> {
    let limit = query.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(Failure::Invalid);
    }
    let after = query.after.as_deref().map(decode_cursor).transpose()?;
    let context = RequestContext::from_request(&request)?;
    let admission = Admission::begin(&ingress, &context).await?;
    let scope = ScopeId::from(admission.workspace.clone());
    let snapshot = ingress
        .stores
        .catalog
        .agent_catalog_snapshot(&scope, &admission.person.owner, 4096)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let visible = visible(&admission, &snapshot, query.selected == Some(true)).await?;
    // Pagination sees only authorized published manifests. Hidden candidates
    // contribute no ids, names, totals, empty pages, cursor or termination.
    let start = match after {
        None => 0,
        Some(id) => visible
            .iter()
            .position(|item| item.id == id)
            .map(|index| index + 1)
            .ok_or(Failure::Invalid)?,
    };
    let mut data = visible
        .into_iter()
        .skip(start)
        .take(limit + 1)
        .collect::<Vec<_>>();
    let next_page = if data.len() > limit {
        data.pop();
        data.last()
            .map(|item| URL_SAFE_NO_PAD.encode(item.id.as_bytes()))
    } else {
        None
    };
    if ingress
        .stores
        .catalog
        .agent_catalog_epoch(&scope)
        .await
        .map_err(|_| Failure::Unavailable)?
        != snapshot.epoch
    {
        return Err(Failure::Unavailable);
    }
    admission.end(&context).await?;
    Ok(Json(Directory {
        data,
        next_page,
        visibility: "current-authorized-published-subset",
    }))
}

pub(super) fn decode_cursor(value: &str) -> Result<String, Failure> {
    if value.len() > 1024 {
        return Err(Failure::Invalid);
    }
    let id = String::from_utf8(
        URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| Failure::Invalid)?,
    )
    .map_err(|_| Failure::Invalid)?;
    if !authority::exact(&id, 300) {
        return Err(Failure::Invalid);
    }
    Ok(id)
}

async fn visible(
    admission: &Admission,
    snapshot: &AgentCatalogSnapshot,
    selected_only: bool,
) -> Result<Vec<Manifest>, Failure> {
    let preferences = snapshot
        .preferences
        .iter()
        .map(|row| (row.agent_id.as_str(), row.state))
        .collect::<BTreeMap<_, _>>();
    let mut visible = Vec::new();
    for entry in &snapshot.entries {
        if entry.head.config.disabled_at.is_some() || entry.head.config.archived_at.is_some() {
            continue;
        }
        let Some(published) = entry.published.as_ref() else {
            continue;
        };
        if published.config.disabled_at.is_some() || published.config.archived_at.is_some() {
            continue;
        }
        let id = &published.config.id;
        if !admission.allows(id, authority::USE).await? {
            continue;
        }
        let selection = if admission.allows(id, authority::SELECTION_READ).await? {
            match preferences.get(id.as_str()) {
                Some(state) if state.selected => Selection::Selected {
                    generation: state.generation,
                },
                state => Selection::NotSelected {
                    generation: state.map_or(0, |state| state.generation),
                },
            }
        } else {
            if selected_only {
                return Err(Failure::Denied);
            }
            Selection::Unknown
        };
        if selected_only && !matches!(&selection, Selection::Selected { .. }) {
            continue;
        }
        visible.push(Manifest {
            id: id.clone(),
            name: published.config.name.clone().unwrap_or_else(|| id.clone()),
            version: published.revision,
            kind: "agent",
            selection,
        });
    }
    Ok(visible)
}

async fn select(
    State(ingress): State<Ingress>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    match select_inner(&ingress, &id, request).await {
        Ok(value) => value,
        Err(error) => error.into_response(),
    }
}
async fn select_inner(ingress: &Ingress, id: &str, request: Request) -> Result<Response, Failure> {
    if !authority::exact(id, 300) {
        return Err(Failure::Invalid);
    }
    let context = RequestContext::from_request(&request)?;
    let admission = Admission::begin(ingress, &context).await?;
    if !admission.allows(id, authority::SELECTION_WRITE).await? {
        return Err(Failure::Denied);
    }
    // Capture request metadata before consuming its bounded body. This same
    // trusted context is used at the ending identity/freshness guard.
    let body = request.into_body();
    let bytes = axum::body::to_bytes(body, 4096)
        .await
        .map_err(|_| Failure::Invalid)?;
    let intent: AgentSelectionIntent =
        serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)?;
    intent.validate().map_err(|_| Failure::Invalid)?;
    if intent.selected {
        if !admission.allows(id, authority::USE).await? {
            return Err(Failure::Denied);
        }
        let scope = ScopeId::from(admission.workspace.clone());
        let snapshot = ingress
            .stores
            .catalog
            .agent_catalog_snapshot(&scope, &admission.person.owner, 4096)
            .await
            .map_err(|_| Failure::Unavailable)?;
        let current = snapshot
            .entries
            .iter()
            .find(|entry| entry.head.config.id == id)
            .ok_or(Failure::Denied)?;
        let published = current.published.as_ref().ok_or(Failure::Denied)?;
        if current.head.config.disabled_at.is_some()
            || current.head.config.archived_at.is_some()
            || published.config.disabled_at.is_some()
            || published.config.archived_at.is_some()
        {
            return Err(Failure::Denied);
        }
        if ingress
            .stores
            .catalog
            .agent_catalog_epoch(&scope)
            .await
            .map_err(|_| Failure::Unavailable)?
            != snapshot.epoch
        {
            return Err(Failure::Unavailable);
        }
    }
    admission.end(&context).await?;
    let result = ingress
        .stores
        .selections
        .set_personal_agent_selection(
            &ScopeId::from(admission.workspace.clone()),
            &admission.person.owner,
            id,
            &intent,
        )
        .await
        .map_err(|_| Failure::Unavailable)?;
    // Once the persistent attempt has been made, loss of current qualification
    // cannot prove that no effect occurred. Expose neither an old receipt nor
    // an editable rejection: retain the original key/body as unknown.
    admission
        .end(&context)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let receipt=match result {
        Ok(receipt)=>receipt,
        Err(AgentSelectionError::Invalid)=>return Err(Failure::Invalid),
        Err(AgentSelectionError::GenerationExhausted)=>return Err(Failure::Unavailable),
        Err(AgentSelectionError::Conflict{current_generation})=>return Ok((StatusCode::CONFLICT,Json(serde_json::json!({"error":"selection-changed","current_generation":current_generation}))).into_response()),
        Err(AgentSelectionError::KeyReuse)=>return Ok((StatusCode::CONFLICT,Json(serde_json::json!({"error":"original-key-content-mismatch"}))).into_response()),
    };
    Ok(Json(serde_json::json!({"state":"observed","receipt":receipt})).into_response())
}

async fn outcome(
    State(ingress): State<Ingress>,
    Path((id, key)): Path<(String, String)>,
    request: Request,
) -> Result<Json<serde_json::Value>, Failure> {
    if !authority::exact(&id, 300) || !authority::exact(&key, 255) {
        return Err(Failure::Invalid);
    }
    let context = RequestContext::from_request(&request)?;
    let admission = Admission::begin(&ingress, &context).await?;
    if !admission.allows(&id, authority::SELECTION_READ).await? {
        return Err(Failure::Denied);
    }
    let receipt = ingress
        .stores
        .selections
        .personal_agent_selection_outcome(
            &ScopeId::from(admission.workspace.clone()),
            &admission.person.owner,
            &id,
            &key,
        )
        .await
        .map_err(|_| Failure::Unavailable)?;
    admission.end(&context).await?;
    Ok(Json(match receipt {
        Some(receipt) => serde_json::json!({"state":"observed","receipt":receipt}),
        None => serde_json::json!({"state":"not-observed","request_key":key}),
    }))
}
