// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: This bounded current-owner projection requires independent
// @formal reason: native selection read and exposes no Agent definition or use grant.
use super::{
    authority::{self, Failure},
    routes::{Admission, Ingress, RequestContext, decode_cursor},
};
use awaken_agent_config::ScopeId;
use axum::{
    Json,
    extract::{Query, Request, State},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectionQuery {
    limit: Option<usize>,
    after: Option<String>,
    selected: Option<bool>,
}

#[derive(Serialize)]
pub(super) struct CurrentSelection {
    agent_id: String,
    generation: u64,
    selected: bool,
}

#[derive(Serialize)]
pub(super) struct SelectionPage {
    data: Vec<CurrentSelection>,
    next_page: Option<String>,
    visibility: &'static str,
}

pub(super) async fn list(
    State(ingress): State<Ingress>,
    Query(query): Query<SelectionQuery>,
    request: Request,
) -> Result<Json<SelectionPage>, Failure> {
    let limit = query.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(Failure::Invalid);
    }
    let after = query.after.as_deref().map(decode_cursor).transpose()?;
    let context = RequestContext::from_request(&request)?;
    let admission = Admission::begin(&ingress, &context).await?;
    let scope = ScopeId::from(context.workspace.clone());
    // The existing physical snapshot owns both preferences and its commit epoch.
    // Its definition rows are never returned, nor do they grant a preference read.
    let snapshot = ingress
        .stores
        .catalog
        .agent_catalog_snapshot(&scope, &admission.person.owner, 4096)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let mut visible = Vec::new();
    for row in &snapshot.preferences {
        if !authority::exact(&row.agent_id, 300) || row.state.generation == 0 {
            return Err(Failure::Unavailable);
        }
        if !admission
            .allows(&row.agent_id, authority::SELECTION_READ)
            .await?
        {
            continue;
        }
        if query
            .selected
            .is_some_and(|selected| selected != row.state.selected)
        {
            continue;
        }
        visible.push(CurrentSelection {
            agent_id: row.agent_id.clone(),
            generation: row.state.generation,
            selected: row.state.selected,
        });
    }
    visible.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
    if visible
        .windows(2)
        .any(|pair| pair[0].agent_id == pair[1].agent_id)
    {
        return Err(Failure::Unavailable);
    }
    // Denied rows cannot supply a cursor, count or empty intermediate page.
    let start = match after {
        None => 0,
        Some(id) => visible
            .iter()
            .position(|row| row.agent_id == id)
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
            .map(|row| URL_SAFE_NO_PAD.encode(row.agent_id.as_bytes()))
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
    Ok(Json(SelectionPage {
        data,
        next_page,
        visibility: "current-owner-authorized-selection-subset",
    }))
}
