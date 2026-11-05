// @formal surface exclude=application_adapter
// @formal owner crates/resources/awaken-resource-application/src/files.rs
// @formal reason: Selects original File provenance for a trusted request and authenticates nobody.
//! One already-admitted personal input request over the original File authority.

use super::FileApplication;
use awaken_resource_contract::PrivateContentBinding;

/// The host must freshly admit identity, Org membership, application binding
/// and native PDP authority before selecting this view. A decoded binding is
/// never a grant. The view shares the original catalog, bytes, lifecycle and
/// clock; it creates no store and changes no other request's authority.
#[must_use]
pub fn admitted_private_file_view(
    application: &FileApplication,
    binding: PrivateContentBinding,
) -> FileApplication {
    let mut view = application.clone();
    view.private_binding = Some(binding);
    view
}
