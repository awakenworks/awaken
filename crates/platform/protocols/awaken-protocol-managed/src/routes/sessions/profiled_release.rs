//! The existing profiled release route's one command-lowering seam.
//!
//! Route observations describe the winning candidate, never command identity.
//! The Session Application compiles the archive record and its idempotency
//! record from stable intent.

pub(super) fn command(
    owner_scope: &str,
    id: &str,
    publication: crate::awaken_extensions::ProfiledSessionRepositoryPublication,
    observed_at: u64,
) -> awaken_session_application::SessionArchiveWithRepositoryPublicationCommand {
    awaken_session_application::SessionArchiveWithRepositoryPublicationCommand::new(
        owner_scope.to_owned(),
        awaken_session_types::SessionId::from(id),
        awaken_session_application::SessionRepositoryPublicationSelector {
            binding_id: awaken_resource_contract::BindingId::new(publication.binding_id),
            expectation: publication.expectation,
        },
        observed_at,
    )
}
