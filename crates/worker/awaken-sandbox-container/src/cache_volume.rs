//! Backend-specific projection of the one neutral `CacheVolume` mount source.

use awaken_provisioning_contract as pc;

pub(crate) const PVC_BIND_REF_PREFIX: &str = "awaken-pvc://";

/// Return a resolved runtime bind when `source` is a CacheVolume; other mount
/// kinds remain owned by the ordinary byte/materialization path.
pub(crate) fn bind_source_ref(
    source: &pc::MountSource,
    mount_path: &str,
    persistent_volume_claims: bool,
) -> Option<Result<String, String>> {
    let pc::MountSource::CacheVolume {
        host_path,
        persistent_volume_claim,
        ..
    } = source
    else {
        return None;
    };
    Some(if persistent_volume_claims {
        persistent_volume_claim
            .as_deref()
            .filter(|claim| !claim.trim().is_empty())
            .map(|claim| format!("{PVC_BIND_REF_PREFIX}{claim}"))
            .ok_or_else(|| {
                format!("Kubernetes CacheVolume `{mount_path}` requires persistent_volume_claim")
            })
    } else if host_path.trim().is_empty() {
        Err(format!(
            "container CacheVolume `{mount_path}` requires host_path"
        ))
    } else {
        Ok(host_path.clone())
    })
}
