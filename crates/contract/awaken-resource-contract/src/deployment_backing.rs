//! Pure selector for deployment-projected Resources backing allocations.

/// The allocation namespace in the deployment backing artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeploymentBackingAllocationKind {
    Database,
    Object,
    Secret,
}

/// The only backing roles understood by the Resources process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeploymentBackingRole {
    ResourcesDatabase,
    FilesObject,
}

/// Select one exact role from the deployment artifact. Kind and role are
/// matched together so an identically named allocation in another namespace
/// cannot acquire database or object authority.
#[must_use]
pub fn select_deployment_backing_role(
    kind: DeploymentBackingAllocationKind,
    role: &[u8],
) -> Option<DeploymentBackingRole> {
    match kind {
        DeploymentBackingAllocationKind::Database if role == b"resources" => {
            Some(DeploymentBackingRole::ResourcesDatabase)
        }
        DeploymentBackingAllocationKind::Object if role == b"files" => {
            Some(DeploymentBackingRole::FilesObject)
        }
        DeploymentBackingAllocationKind::Database
        | DeploymentBackingAllocationKind::Object
        | DeploymentBackingAllocationKind::Secret => None,
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_kind(value: u8) -> DeploymentBackingAllocationKind {
        match value % 3 {
            0 => DeploymentBackingAllocationKind::Database,
            1 => DeploymentBackingAllocationKind::Object,
            _ => DeploymentBackingAllocationKind::Secret,
        }
    }

    #[kani::proof]
    fn deployment_backing_selects_only_exact_resources_database_and_files_object_roles() {
        let kind = symbolic_kind(kani::any());
        let bytes: [u8; 10] = kani::any();
        let length: usize = kani::any();
        kani::assume(length <= bytes.len());
        let role = &bytes[..length];
        let selected = select_deployment_backing_role(kind, role);

        assert_eq!(
            selected == Some(DeploymentBackingRole::ResourcesDatabase),
            kind == DeploymentBackingAllocationKind::Database && role == b"resources"
        );
        assert_eq!(
            selected == Some(DeploymentBackingRole::FilesObject),
            kind == DeploymentBackingAllocationKind::Object && role == b"files"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_exact_role_are_both_binding() {
        assert_eq!(
            select_deployment_backing_role(DeploymentBackingAllocationKind::Database, b"resources"),
            Some(DeploymentBackingRole::ResourcesDatabase)
        );
        assert_eq!(
            select_deployment_backing_role(DeploymentBackingAllocationKind::Object, b"files"),
            Some(DeploymentBackingRole::FilesObject)
        );
        assert_eq!(
            select_deployment_backing_role(DeploymentBackingAllocationKind::Object, b"resources"),
            None
        );
        assert_eq!(
            select_deployment_backing_role(DeploymentBackingAllocationKind::Database, b"files"),
            None
        );
        assert_eq!(
            select_deployment_backing_role(
                DeploymentBackingAllocationKind::Database,
                b"resources-admin"
            ),
            None
        );
    }
}
