// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: The existing bounded identity laws stay pure and have one
// @formal reason: definition shared by package projections and evidence decoding.
//! Canonical bounded image, metadata and fingerprint identities.

use super::{K8sPackageRealizationError, MAX_EVIDENCE_TEXT_BYTES};
use awaken_container_contract::RuntimeError;
use awaken_container_contract::RuntimeError::Backend;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

pub(super) fn destination_repository<'a>(
    destination: &'a str,
    fingerprint: &str,
) -> Result<&'a str, K8sPackageRealizationError> {
    let suffix = format!(":{fingerprint}");
    let repository = destination
        .strip_suffix(&suffix)
        .filter(|repository| bounded_text(repository) && !repository.contains('@'))
        .ok_or_else(|| {
            K8sPackageRealizationError::invalid(
                "package destination does not bind the exact recipe fingerprint",
            )
        })?;
    Ok(repository)
}

pub(crate) fn immutable_image(value: &str) -> Result<(&str, &str), K8sPackageRealizationError> {
    if !bounded_text(value) {
        return Err(K8sPackageRealizationError::invalid(
            "immutable image is empty or unbounded",
        ));
    }
    let (repository, digest) = value.rsplit_once("@sha256:").ok_or_else(|| {
        K8sPackageRealizationError::invalid("image is not an immutable sha256 reference")
    })?;
    if repository.is_empty()
        || repository.contains('@')
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(K8sPackageRealizationError::invalid(
            "image is not one canonical immutable sha256 reference",
        ));
    }
    Ok((repository, digest))
}

pub(super) fn image_id(value: &str) -> Result<&str, K8sPackageRealizationError> {
    let image = value.split_once("://").map_or(value, |(_, image)| image);
    immutable_image(image)?;
    Ok(image)
}

pub(super) fn annotation<'a>(
    metadata: &'a ObjectMeta,
    key: &str,
) -> Result<&'a str, K8sPackageRealizationError> {
    metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .map(String::as_str)
        .filter(|value| bounded_text(value))
        .ok_or_else(|| {
            K8sPackageRealizationError::invalid(format!("object has no bounded `{key}` annotation"))
        })
}

pub(super) fn metadata_uid(
    metadata: &ObjectMeta,
    kind: &str,
) -> Result<String, K8sPackageRealizationError> {
    metadata
        .uid
        .as_deref()
        .filter(|uid| bounded_text(uid))
        .map(ToOwned::to_owned)
        .ok_or_else(|| K8sPackageRealizationError::invalid(format!("{kind} has no bounded UID")))
}

pub(super) fn object_namespace(metadata: &ObjectMeta, namespace: &str) -> bool {
    metadata.namespace.as_deref() == Some(namespace)
}

pub(super) fn runtime_error(message: impl Into<String>) -> RuntimeError {
    Backend(message.into())
}

pub(super) fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn bounded_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_EVIDENCE_TEXT_BYTES
        && !value.chars().any(char::is_control)
}

pub(super) fn has_no_owner_references(metadata: &ObjectMeta) -> bool {
    metadata.owner_references.as_ref().is_none_or(Vec::is_empty)
}
