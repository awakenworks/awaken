use super::*;

/// Provider-neutral, secret-free completion receipt shared by Local and
/// Namespace. Provider adapters validate it against their exact effective spec
/// before projecting a replayed live object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RealizationCompletionReceipt {
    mounts: Vec<RealizedMountRecord>,
    memory_materializations: Vec<pc::MemoryMaterializationEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RealizedMountRecord {
    mount_id: String,
    mount_path: String,
    access: pc::MountAccess,
    realization: pc::Realization,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
}

impl RealizationCompletionReceipt {
    pub(crate) fn new(
        mounts: &[pc::RealizedMount],
        mut memory_materializations: Vec<pc::MemoryMaterializationEvidence>,
    ) -> Result<Self, pc::SandboxError> {
        let mut mount_ids = std::collections::BTreeSet::new();
        let mut mount_paths = std::collections::BTreeSet::new();
        let mounts = mounts
            .iter()
            .map(|mount| {
                if mount.mount_id.trim().is_empty()
                    || mount.mount_path.trim().is_empty()
                    || mount
                        .content_hash
                        .as_deref()
                        .is_some_and(|hash| hash.trim().is_empty())
                    || !mount_ids.insert(mount.mount_id.as_str())
                    || !mount_paths.insert(mount.mount_path.as_str())
                {
                    return Err(err(
                        "filesystem realization receipt has invalid or duplicate mounts",
                    ));
                }
                Ok(RealizedMountRecord {
                    mount_id: mount.mount_id.clone(),
                    mount_path: mount.mount_path.clone(),
                    access: mount.access,
                    realization: mount.realization,
                    content_hash: mount.content_hash.clone(),
                })
            })
            .collect::<Result<Vec<_>, pc::SandboxError>>()?;
        for evidence in &memory_materializations {
            evidence.validate()?;
        }
        memory_materializations.sort_by(|left, right| {
            (&left.mount_path, &left.store_id).cmp(&(&right.mount_path, &right.store_id))
        });
        if memory_materializations.windows(2).any(|pair| {
            pair[0].mount_path == pair[1].mount_path && pair[0].store_id == pair[1].store_id
        }) {
            return Err(err(
                "filesystem realization receipt has duplicate Memory materializations",
            ));
        }
        Ok(Self {
            mounts,
            memory_materializations,
        })
    }

    pub(super) fn validate(&self) -> Result<(), pc::SandboxError> {
        let mounts = self
            .mounts
            .iter()
            .map(|mount| pc::RealizedMount {
                mount_id: mount.mount_id.clone(),
                mount_path: mount.mount_path.clone(),
                access: mount.access,
                realization: mount.realization,
                content_hash: mount.content_hash.clone(),
            })
            .collect::<Vec<_>>();
        let canonical = Self::new(&mounts, self.memory_materializations.clone())?;
        if canonical == *self {
            Ok(())
        } else {
            Err(err(
                "filesystem realization receipt is not canonically encoded",
            ))
        }
    }

    pub(crate) fn mounts(&self) -> Vec<pc::RealizedMount> {
        self.mounts
            .iter()
            .map(|mount| pc::RealizedMount {
                mount_id: mount.mount_id.clone(),
                mount_path: mount.mount_path.clone(),
                access: mount.access,
                realization: mount.realization,
                content_hash: mount.content_hash.clone(),
            })
            .collect()
    }

    pub(crate) fn memory_materializations(&self) -> &[pc::MemoryMaterializationEvidence] {
        &self.memory_materializations
    }
}
