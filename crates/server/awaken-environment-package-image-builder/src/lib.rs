//! Coordinator adapter from an exact Environment build demand to the existing
//! container package-image provisioner. It owns no job state, lease, cache, or
//! registry client; those remain in their authoritative components.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_environment_realization_contract::{
    EnvironmentImageBuildDemand, EnvironmentImageBuildError, EnvironmentImageBuilder,
};
use awaken_provisioning_contract::{NetworkPolicy, PackageRequirements};

fn package_requirements(demand: &EnvironmentImageBuildDemand) -> PackageRequirements {
    PackageRequirements {
        managers: demand
            .config
            .packages()
            .manager_packages()
            .into_iter()
            .filter(|(_, packages)| !packages.is_empty())
            .map(|(manager, packages)| (manager.to_owned(), packages.to_vec()))
            .collect(),
        // Unpinned dependency refresh is frozen by the authoritative recipe,
        // not by metadata-only Environment revisions.
        resolution_id: Some(demand.build_key.clone()),
    }
}

fn build_network_policy(config: &awaken_environment_contract::EnvironmentConfig) -> NetworkPolicy {
    use awaken_environment_contract::EnvironmentNetworking;
    match config {
        awaken_environment_contract::EnvironmentConfig::SelfHosted
        | awaken_environment_contract::EnvironmentConfig::Cloud {
            networking: EnvironmentNetworking::Unrestricted,
            ..
        } => NetworkPolicy::Unrestricted,
        awaken_environment_contract::EnvironmentConfig::Cloud {
            networking:
                EnvironmentNetworking::Limited {
                    allowed_hosts,
                    allow_package_managers,
                    ..
                },
            ..
        } => {
            let mut hosts = allowed_hosts.clone();
            if *allow_package_managers {
                hosts.extend(
                    awaken_environment_contract::PUBLIC_PACKAGE_REGISTRY_HOSTS
                        .iter()
                        .map(ToString::to_string),
                );
            }
            hosts.sort();
            hosts.dedup();
            NetworkPolicy::Allowlist { hosts }
        }
    }
}

struct PackageEnvironmentImageBuilder {
    provisioner: Arc<dyn awaken_sandbox_container::PackageImageProvisioner>,
}

#[async_trait]
impl EnvironmentImageBuilder for PackageEnvironmentImageBuilder {
    async fn base_image_identity(
        &self,
        reference: &str,
    ) -> Result<String, EnvironmentImageBuildError> {
        self.provisioner
            .package_base_image_identity(reference)
            .await
            .map_err(|error| EnvironmentImageBuildError::Unavailable(error.to_string()))
    }

    async fn build(
        &self,
        demand: &EnvironmentImageBuildDemand,
    ) -> Result<String, EnvironmentImageBuildError> {
        let packages = package_requirements(demand);
        let network = build_network_policy(&demand.config);
        self.provisioner
            .prepare_package_image(&demand.base_image, &packages, &network)
            .await
            .map_err(|error| EnvironmentImageBuildError::Unavailable(error.to_string()))
    }

    async fn available(&self, image: &str) -> Result<bool, EnvironmentImageBuildError> {
        self.provisioner
            .package_image_available(image)
            .await
            .map_err(|error| EnvironmentImageBuildError::Unavailable(error.to_string()))
    }
}

/// Bind the Environment build application to the already-authoritative package
/// image provisioning implementation.
#[must_use]
pub fn package_environment_image_builder(
    provisioner: Arc<dyn awaken_sandbox_container::PackageImageProvisioner>,
) -> Arc<dyn EnvironmentImageBuilder> {
    Arc::new(PackageEnvironmentImageBuilder { provisioner })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_environment_contract::{
        EnvironmentConfig, EnvironmentNetworking, EnvironmentPackages, EnvironmentRevision,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingProvisioner {
        calls: Mutex<
            Vec<(
                String,
                awaken_provisioning_contract::PackageRequirements,
                awaken_provisioning_contract::NetworkPolicy,
            )>,
        >,
    }

    #[async_trait]
    impl awaken_sandbox_container::PackageImageProvisioner for RecordingProvisioner {
        async fn package_base_image_identity(
            &self,
            reference: &str,
        ) -> Result<String, awaken_sandbox_container::RuntimeError> {
            Ok(format!("{reference}@sha256:resolved"))
        }

        async fn prepare_package_image(
            &self,
            base_image: &str,
            packages: &awaken_provisioning_contract::PackageRequirements,
            network: &awaken_provisioning_contract::NetworkPolicy,
        ) -> Result<String, awaken_sandbox_container::RuntimeError> {
            self.calls.lock().unwrap().push((
                base_image.to_owned(),
                packages.clone(),
                network.clone(),
            ));
            Ok("registry/awaken@sha256:prepared".into())
        }

        async fn package_image_available(
            &self,
            image: &str,
        ) -> Result<bool, awaken_sandbox_container::RuntimeError> {
            Ok(image == "registry/awaken@sha256:prepared")
        }
    }

    #[tokio::test]
    async fn exact_environment_demand_projects_once_to_package_provisioning() {
        // Cause/effect decision table: R0 base identity resolution delegates to
        // the authoritative provisioner; R1 exact packages preserve canonical
        // manager names/values and revision-scoped resolution; R2 limited
        // package-manager networking becomes the normalized allowlist; R3 the
        // provisioner's immutable reference and availability are returned
        // without another build-state or cache implementation.
        let provisioner = Arc::new(RecordingProvisioner::default());
        let builder = package_environment_image_builder(provisioner.clone());
        let demand = EnvironmentImageBuildDemand {
            build_key: "build-1".into(),
            environment_id: "env-browser".into(),
            source_revision: EnvironmentRevision(7),
            definition_fingerprint: "definition-fp".into(),
            base_image: "registry/base@sha256:exact".into(),
            config: EnvironmentConfig::Cloud {
                networking: EnvironmentNetworking::Limited {
                    allowed_hosts: vec!["api.example".into()],
                    allow_mcp_servers: false,
                    allow_package_managers: true,
                },
                packages: EnvironmentPackages {
                    npm: vec!["@playwright/mcp@latest".into()],
                    ..Default::default()
                },
            },
        };
        assert_eq!(
            builder
                .base_image_identity("registry/base:latest")
                .await
                .unwrap(),
            "registry/base:latest@sha256:resolved",
            "R0"
        );
        assert_eq!(
            builder.build(&demand).await.unwrap(),
            "registry/awaken@sha256:prepared",
            "R3"
        );
        let (base, packages, network) = {
            let calls = provisioner.calls.lock().unwrap();
            calls[0].clone()
        };
        assert_eq!(base, "registry/base@sha256:exact", "R1");
        assert_eq!(
            packages.managers.get("npm").unwrap(),
            &["@playwright/mcp@latest"],
            "R1"
        );
        assert_eq!(
            packages.resolution_id.as_deref(),
            Some(demand.build_key.as_str()),
            "R1"
        );
        assert!(
            matches!(
                &network,
                awaken_provisioning_contract::NetworkPolicy::Allowlist { hosts }
                    if hosts.contains(&"api.example".into())
                        && hosts.contains(&"registry.npmjs.org".into())
            ),
            "R2"
        );
        assert!(
            builder
                .available("registry/awaken@sha256:prepared")
                .await
                .unwrap(),
            "R3"
        );
    }
}
