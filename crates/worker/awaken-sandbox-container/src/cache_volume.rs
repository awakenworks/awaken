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
    let pc::MountSource::CacheVolume { location, .. } = source else {
        return None;
    };
    Some(match (persistent_volume_claims, location) {
        (true, pc::CacheVolumeLocation::PersistentVolumeClaim { claim_name }) => {
            Ok(format!("{PVC_BIND_REF_PREFIX}{claim_name}"))
        }
        (false, pc::CacheVolumeLocation::HostPath { path }) => Ok(path.clone()),
        (true, pc::CacheVolumeLocation::HostPath { .. }) => Err(format!(
            "Kubernetes CacheVolume `{mount_path}` requires a persistent volume claim"
        )),
        (false, pc::CacheVolumeLocation::PersistentVolumeClaim { .. }) => Err(format!(
            "container CacheVolume `{mount_path}` requires a host path"
        )),
    })
}
