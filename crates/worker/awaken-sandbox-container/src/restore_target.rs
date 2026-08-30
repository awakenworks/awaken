//! Exact restoration-target projection for the container provider.
//!
//! This module owns restore metadata labels, stable target identity, and the
//! retained host-staging namespace. Physical recover/create/dispose sequencing
//! remains solely in `provider_realization`; durable-handle decoding remains in
//! `recovery`.

use super::{ContainerPlan, LIVE_INPUTS_ROOT, RootfsPlan, RuntimeError, StagingGuard, pc};

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RESTORE_EFFECT_LABEL: &str = "awaken.sandbox.restore.effect";
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RESTORE_GENERATION_LABEL: &str = "awaken.sandbox.restore.generation";
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RESTORE_CHECKPOINT_LABEL: &str = "awaken.sandbox.restore.checkpoint";
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RESTORE_CHECKPOINT_DIGEST_LABEL: &str = "awaken.sandbox.restore.checkpoint-digest";
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RESTORE_SPEC_LABEL: &str = "awaken.sandbox.restore.spec";
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RESTORE_EXCLUSIONS_LABEL: &str = "awaken.sandbox.restore.exclusions";
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RESTORE_PLAN_LABEL: &str = "awaken.sandbox.restore.plan";

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn restoration_metadata(
    evidence: &pc::SandboxRestorationEvidence,
) -> [(&'static str, &str); 6] {
    [
        (RESTORE_EFFECT_LABEL, evidence.effect_id()),
        (RESTORE_GENERATION_LABEL, evidence.generation_id()),
        (RESTORE_CHECKPOINT_LABEL, evidence.checkpoint_id()),
        (
            RESTORE_CHECKPOINT_DIGEST_LABEL,
            evidence.checkpoint_digest(),
        ),
        (RESTORE_SPEC_LABEL, evidence.sandbox_spec_fingerprint()),
        (
            RESTORE_EXCLUSIONS_LABEL,
            evidence.checkpoint_exclusions_fingerprint(),
        ),
    ]
}

pub(crate) fn restoration_plan_fingerprint(plan: &ContainerPlan) -> String {
    let mut canonical = plan.clone();
    canonical
        .binds
        .retain(|bind| bind.mount_path != LIVE_INPUTS_ROOT);
    if !canonical.packages.is_empty() {
        canonical.image = "<awaken-package-derived-image>".into();
        if matches!(canonical.rootfs, RootfsPlan::Image(_)) {
            canonical.rootfs = RootfsPlan::Image("<awaken-package-derived-image>".into());
        }
    }
    blake3::hash(format!("container-restore-plan-v1\0{canonical:?}").as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn restoration_evidence_from_metadata(
    mut value: impl FnMut(&str) -> Option<String>,
    substrate: &str,
) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
    let effect_id = value(RESTORE_EFFECT_LABEL);
    let generation_id = value(RESTORE_GENERATION_LABEL);
    let checkpoint_id = value(RESTORE_CHECKPOINT_LABEL);
    let checkpoint_digest = value(RESTORE_CHECKPOINT_DIGEST_LABEL);
    let sandbox_spec_fingerprint = value(RESTORE_SPEC_LABEL);
    let checkpoint_exclusions_fingerprint = value(RESTORE_EXCLUSIONS_LABEL);
    match (
        effect_id,
        generation_id,
        checkpoint_id,
        checkpoint_digest,
        sandbox_spec_fingerprint,
        checkpoint_exclusions_fingerprint,
    ) {
        (None, None, None, None, None, None) => Ok(None),
        (
            Some(effect_id),
            Some(generation_id),
            Some(checkpoint_id),
            Some(checkpoint_digest),
            Some(sandbox_spec_fingerprint),
            Some(checkpoint_exclusions_fingerprint),
        ) => pc::SandboxRestorationEvidence::from_exact_parts(
            effect_id,
            generation_id,
            checkpoint_id,
            checkpoint_digest,
            sandbox_spec_fingerprint,
            checkpoint_exclusions_fingerprint,
        )
        .map(Some)
        .map_err(|error| RuntimeError::Backend(error.to_string())),
        _ => Err(RuntimeError::Backend(format!(
            "{substrate} has incomplete restore evidence"
        ))),
    }
}

pub(super) fn retained_host_staging(
    handle: Option<&pc::ContainerContinuationHandle>,
) -> Result<Option<StagingGuard>, RuntimeError> {
    let Some(pc::ContainerContinuationHandle::HostBindRestoration(locator)) = handle else {
        return Ok(None);
    };
    retained_host_staging_path(std::path::PathBuf::from(locator.staging_root())).map(Some)
}

fn retained_host_staging_path(path: std::path::PathBuf) -> Result<StagingGuard, RuntimeError> {
    validate_host_staging_path(&path)?;
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| RuntimeError::Backend(error.to_string()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(RuntimeError::Backend(
            "restored host-bind staging locator is not a physical directory".into(),
        ));
    }
    Ok(StagingGuard::retained(path))
}

fn validate_host_staging_path(path: &std::path::Path) -> Result<(), RuntimeError> {
    let provider_temp = std::env::temp_dir();
    let trusted_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("awaken-acp-stage-"));
    if path.parent() != Some(provider_temp.as_path()) || !trusted_name {
        return Err(RuntimeError::Backend(
            "restored host-bind staging locator is outside the provider staging namespace".into(),
        ));
    }
    Ok(())
}

#[cfg(any(test, feature = "docker", feature = "podman"))]
pub(super) fn remove_host_staging_path(path: &std::path::Path) -> Result<(), RuntimeError> {
    validate_host_staging_path(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path).map_err(|error| RuntimeError::Backend(error.to_string()))
        }
        Ok(_) => {
            std::fs::remove_file(path).map_err(|error| RuntimeError::Backend(error.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::Backend(error.to_string())),
    }
}

pub(super) fn restoration_runtime_scope(
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<String, pc::SandboxError> {
    evidence.physical_target_key()
}

#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) fn restore_container_name(scope: &str) -> String {
    let identity = blake3::hash(scope.as_bytes()).to_hex();
    format!("awaken-restore-{identity}")
}
