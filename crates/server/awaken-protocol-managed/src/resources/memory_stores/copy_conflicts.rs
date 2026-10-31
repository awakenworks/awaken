//! Awaken extension: inspect retained candidates without changing the vendor
//! Memory/version schema. Uses the same store visibility and page authority.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

#[derive(Clone, Serialize)]
struct CopyConflictObject {
    id: String,
    path: String,
    base: Option<awaken_resource_contract::MemoryCopyBase>,
    proposed_operation: &'static str,
    content: Option<String>,
}

pub(super) async fn list(
    State(state): State<Arc<MemoryStoreApi>>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    ManagedQuery(query): ManagedQuery<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let view = match MemoryView::parse(&query, MemoryView::Basic) {
        Ok(view) => view,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    let page = match parse_page_query(&query) {
        Ok(page) => page,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    match active_store_exists(&state, &workspace, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found("memory_store"),
        Err(error) => return application_error(error),
    }
    let candidates = match state.conflicts.list_copy_conflicts(&id).await {
        Ok(candidates) => candidates,
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let mut data: Vec<_> = candidates
        .into_iter()
        .map(|candidate| CopyConflictObject {
            id: candidate.id(),
            proposed_operation: if candidate.content.is_some() {
                "write"
            } else {
                "delete"
            },
            path: candidate.path,
            base: candidate.base,
            content: if view.includes_content() {
                candidate.content
            } else {
                None
            },
        })
        .collect();
    data.sort_by(|left, right| left.id.cmp(&right.id));
    (
        StatusCode::OK,
        Json(paginate_by(data, &page, |candidate| candidate.id.clone())),
    )
        .into_response()
}
