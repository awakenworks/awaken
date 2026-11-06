// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: Scoped configuration-version reads project existing immutable revisions and publications.
//! Complete Console reads; the official Managed projection remains unchanged.

use std::collections::HashSet;

use awaken_agent_config::AgentConfigRevision;
use awaken_config_service::ConfigPlane;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{RequiredScope, managed_from_agent_config};
use crate::agent_revision_visibility::revision_is_visible;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VersionQuery {
    limit: Option<usize>,
    page: Option<u64>,
}

struct ValidatedVersionQuery {
    limit: usize,
    page: Option<u64>,
}

impl VersionQuery {
    fn validate(&self) -> Result<ValidatedVersionQuery, String> {
        let limit = self.limit.unwrap_or(100);
        if !(1..=500).contains(&limit) || self.page == Some(0) {
            return Err("limit must be between 1 and 500; page must be a positive revision".into());
        }
        Ok(ValidatedVersionQuery {
            limit,
            page: self.page,
        })
    }
}

fn version_page(
    mut revisions: Vec<AgentConfigRevision>,
    published: &HashSet<(String, u64)>,
    query: &ValidatedVersionQuery,
) -> Result<Value, String> {
    let limit = query.limit;
    let head = revisions.iter().map(|row| row.revision).max();
    revisions.retain(|row| {
        revision_is_visible(
            row.config.lifecycle(),
            published.contains(&(row.config.id.clone(), row.revision)),
            head == Some(row.revision),
        ) && query.page.is_none_or(|before| row.revision < before)
    });
    revisions.sort_by_key(|row| std::cmp::Reverse(row.revision));
    let more = revisions.len() > limit;
    revisions.truncate(limit);
    let next = more.then(|| {
        revisions
            .last()
            .expect("nonzero bounded page")
            .revision
            .to_string()
    });
    let data = revisions
        .into_iter()
        .map(|row| {
            let is_published = published.contains(&(row.config.id.clone(), row.revision));
            let mut body = managed_from_agent_config(&row.config, is_published)?;
            // This is the existing complete configuration, not a Managed model-id
            // approximation. Preserve endpoint ids, ACP configuration and Auto.
            body["model"] = serde_json::to_value(&row.config.model_binding)
                .map_err(|error| error.to_string())?;
            body["version"] = json!(row.revision);
            body["created_at"] = json!(row.created_at_unix_ms);
            body["updated_at"] = json!(row.updated_at_unix_ms);
            Ok(body)
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok(json!({"data": data, "next_page": next}))
}

pub(super) async fn list_versions(
    State(plane): State<ConfigPlane>,
    RequiredScope(scope): RequiredScope,
    Path(id): Path<String>,
    Query(query): Query<VersionQuery>,
) -> (StatusCode, Json<Value>) {
    let query = match query.validate() {
        Ok(query) => query,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid version page"})),
            );
        }
    };
    let revisions = match plane.list_revisions(&scope, &id).await {
        Ok(rows) if !rows.is_empty() => rows,
        Ok(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Agent configuration not found"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };
    let published = match plane.published_revisions(&scope).await {
        Ok(published) => published,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };
    match version_page(revisions, &published, &query) {
        Ok(page) => (StatusCode::OK, Json(page)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_routes::test_support::agent_config;

    fn row(revision: u64) -> AgentConfigRevision {
        AgentConfigRevision {
            config: agent_config("versions"),
            revision,
            created_at_unix_ms: Some(1),
            updated_at_unix_ms: Some(revision),
        }
    }

    #[test]
    fn exact_targets_and_opaque_configuration_survive_the_bounded_read() {
        // DT-CONFIG R1 published exact endpoint/ACP options -> full unchanged
        // selection in the read, not a lossy Managed string. R2 newer draft ->
        // absent from published tests. Reads never rewrite the source config.
        let mut published = row(2);
        published.config.model_binding = serde_json::from_value(json!({
            "mode":"target", "backend_ref":"acp:codex",
            "target":{"model_id":"model", "provider_id":"provider", "protocol_endpoint_id":"exact-endpoint"},
            "configuration":{"working_directory":"repo"}
        })).expect("canonical selection");
        let expected = serde_json::to_value(&published.config.model_binding).unwrap();
        let page = version_page(
            vec![row(3), published],
            &HashSet::from([("versions".into(), 2)]),
            &VersionQuery::default().validate().unwrap(),
        )
        .unwrap();
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
        assert_eq!(page["data"][0]["version"], 2);
        assert_eq!(page["data"][0]["model"], expected);
    }

    #[test]
    fn current_lifecycle_and_cursor_keep_their_exact_coordinates() {
        // R3 current disabled -> visible but never a published test candidate;
        // historical terminal rows -> hidden. R4 descending bounded pages use
        // the last exact revision as cursor without overlap or rounded ids.
        let mut head = row(5);
        head.config.disabled_at = Some("2026-10-22T00:00:00Z".into());
        let mut past = row(3);
        past.config.disabled_at = head.config.disabled_at.clone();
        let rows = vec![row(1), row(2), past, row(4), head];
        let published = HashSet::from([
            ("versions".into(), 1),
            ("versions".into(), 2),
            ("versions".into(), 4),
        ]);
        let first = version_page(
            rows.clone(),
            &published,
            &VersionQuery {
                limit: Some(2),
                page: None,
            }
            .validate()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            first["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["version"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [5, 4]
        );
        assert_eq!(first["next_page"], "4");
        let next = version_page(
            rows,
            &published,
            &VersionQuery {
                limit: Some(2),
                page: Some(4),
            }
            .validate()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            next["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["version"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert!(next["next_page"].is_null());
    }

    #[test]
    fn invalid_pages_never_invent_a_revision() {
        // R5 invalid page bounds/cursor -> reject, never clamp to another
        // publication or silently return an unbounded version list. One checked
        // request type owns these rules before storage; the projection cannot
        // introduce a second pagination validator. R7 unknown
        // query keys -> reject rather than discard an authored pagination field.
        assert!(serde_json::from_value::<VersionQuery>(json!({"before": 2})).is_err());
        for query in [
            VersionQuery {
                limit: Some(0),
                page: None,
            },
            VersionQuery {
                limit: Some(501),
                page: None,
            },
            VersionQuery {
                limit: None,
                page: Some(0),
            },
        ] {
            assert!(query.validate().is_err());
        }
    }

    #[test]
    fn automatic_and_profile_history_remain_configuration_values() {
        // R6 Auto/Profile have no need for a fake Managed model id in this
        // read. Preserve authored intent; execution still uses its already
        // frozen publication rather than resolving the current profile here.
        for model in [
            json!({"mode":"auto"}),
            json!({"mode":"profile", "profile_id":"approved"}),
        ] {
            let mut revision = row(1);
            revision.config.model_binding = serde_json::from_value(model.clone()).unwrap();
            let page = version_page(
                vec![revision],
                &HashSet::from([("versions".into(), 1)]),
                &VersionQuery::default().validate().unwrap(),
            )
            .unwrap();
            assert_eq!(page["data"][0]["model"], model);
        }
    }
}
