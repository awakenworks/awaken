//! Read-only Repository reference projection for Session-owned retirement.

use awaken_session_contract::{
    ResolvedInputSource, ResolvedSessionResources, SessionResourceState,
};

pub(super) fn session_resources_reference_repository(
    resources: &SessionResourceState,
    repository_id: &str,
) -> bool {
    resources
        .resource_references()
        .inputs()
        .iter()
        .any(|input| repository_input_matches(&input.source, repository_id))
}

pub(super) fn session_resources_reference_repository_generation(
    resources: &ResolvedSessionResources,
    repository_id: &str,
) -> bool {
    resources
        .inputs()
        .iter()
        .any(|input| repository_input_matches(&input.source, repository_id))
}

fn repository_input_matches(source: &ResolvedInputSource, repository_id: &str) -> bool {
    matches!(
        source,
        ResolvedInputSource::Repository {
            repository_id: candidate,
            ..
        } if candidate.as_str() == repository_id
    )
}
