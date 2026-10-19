// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Static owner: the sole build-stage projection from a canonical non-self-hosted
// @formal reason: package-image demand to the existing package provisioner. Dynamic path: canonical demand
// @formal reason: admission excludes SelfHosted and empty packages; Cloud Unrestricted projects to
// @formal reason: NetworkPolicy::Unrestricted, while Cloud Limited preserves allowed_hosts, independently of runtime registry permission
// @formal reason: adds the Environment owner's PUBLIC_PACKAGE_REGISTRY_HOSTS for declared packages, sorts and deduplicates them,
// @formal reason: and supplies the same policy to build and availability. The Environment contract owns
// @formal reason: authored networking and registry-catalog semantics, while the provisioner owns
// @formal reason: enforcement; this is a runtime.capabilities.capability_floor product-requirement
// @formal reason: classification, not a direct formal-proof claim.
//! Coordinator adapter from an exact Environment build demand to the existing
//! container package-image provisioner. It owns no job state, lease, cache, or
//! registry client; those remain in their authoritative components.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_environment_contract::PackageRequirements;
use awaken_executable_environment_contract::{
    EnvironmentImageBuildDemand, EnvironmentImageBuildError, EnvironmentImageBuilder,
};
use awaken_provisioning_contract::NetworkPolicy;

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
    config
        .package_build_network_hosts()
        .map_or(NetworkPolicy::Unrestricted, |hosts| {
            NetworkPolicy::Allowlist { hosts }
        })
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

    async fn available(
        &self,
        demand: &EnvironmentImageBuildDemand,
        image: &str,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let packages = package_requirements(demand);
        let network = build_network_policy(&demand.config);
        self.provisioner
            .package_image_available(&demand.base_image, &packages, &network, image)
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
                awaken_environment_contract::PackageRequirements,
                awaken_provisioning_contract::NetworkPolicy,
            )>,
        >,
        availability_calls: Mutex<
            Vec<(
                String,
                awaken_environment_contract::PackageRequirements,
                awaken_provisioning_contract::NetworkPolicy,
                String,
            )>,
        >,
    }

    #[async_trait]
    impl awaken_sandbox_container::PackageImageProvisioner for RecordingProvisioner {
        async fn probe_builder(&self) -> Result<(), awaken_container_contract::RuntimeError> {
            Ok(())
        }

        async fn package_base_image_identity(
            &self,
            reference: &str,
        ) -> Result<String, awaken_container_contract::RuntimeError> {
            Ok(format!("{reference}@sha256:resolved"))
        }

        async fn prepare_package_image(
            &self,
            base_image: &str,
            packages: &awaken_environment_contract::PackageRequirements,
            network: &awaken_provisioning_contract::NetworkPolicy,
        ) -> Result<String, awaken_container_contract::RuntimeError> {
            self.calls.lock().unwrap().push((
                base_image.to_owned(),
                packages.clone(),
                network.clone(),
            ));
            Ok("registry/awaken@sha256:prepared".into())
        }

        async fn package_image_available(
            &self,
            base_image: &str,
            packages: &awaken_environment_contract::PackageRequirements,
            network: &awaken_provisioning_contract::NetworkPolicy,
            image: &str,
        ) -> Result<bool, awaken_container_contract::RuntimeError> {
            self.availability_calls.lock().unwrap().push((
                base_image.to_owned(),
                packages.clone(),
                network.clone(),
                image.to_owned(),
            ));
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
        // without another build-state or cache implementation; R4 availability
        // receives the exact same recipe inputs plus the stored immutable image,
        // so the adapter cannot create a second build executor or identity path.
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
                .available(&demand, "registry/awaken@sha256:prepared")
                .await
                .unwrap(),
            "R4"
        );
        assert_eq!(
            provisioner.availability_calls.lock().unwrap().as_slice(),
            [(
                "registry/base@sha256:exact".into(),
                packages,
                network,
                "registry/awaken@sha256:prepared".into(),
            )],
            "R4 exact demand and stored identity reuse the one availability port"
        );
    }
    #[tokio::test]
    async fn declared_packages_prepare_over_the_same_policy_without_runtime_registry_access() {
        // DT-BN: C1 declared npm input; C2 empty limited runtime policy with
        // registry permission off/on. BN1 both rows build/check the same
        // declared registry policy; BN2 only C2 changes runtime hosts. The
        // provisioner remains responsible for no-bypass enforcement and exact
        // image verification; this port fixture proves neither by itself.
        for allow_package_managers in [false, true] {
            let provisioner = Arc::new(RecordingProvisioner::default());
            let builder = package_environment_image_builder(provisioner.clone());
            let demand = EnvironmentImageBuildDemand {
                build_key: "offline-build".into(),
                environment_id: "env-offline".into(),
                source_revision: EnvironmentRevision(1),
                definition_fingerprint: "definition".into(),
                base_image: "registry/base@sha256:exact".into(),
                config: EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Limited {
                        allowed_hosts: Vec::new(),
                        allow_mcp_servers: false,
                        allow_package_managers,
                    },
                    packages: EnvironmentPackages {
                        npm: vec!["tsx@4.0.0".into()],
                        ..Default::default()
                    },
                },
            };
            demand.config.validate().unwrap();
            assert_eq!(
                !demand.config.runtime_network_hosts().unwrap().is_empty(),
                allow_package_managers,
                "BN2"
            );
            let image = builder.build(&demand).await.unwrap();
            assert!(builder.available(&demand, &image).await.unwrap());
            let build = provisioner.calls.lock().unwrap()[0].clone();
            let checked = provisioner.availability_calls.lock().unwrap()[0].clone();
            assert_eq!(
                (&build.0, &build.1, &build.2),
                (&checked.0, &checked.1, &checked.2),
                "BN1 exact build/check input"
            );
            assert!(
                matches!(&build.2, NetworkPolicy::Allowlist { hosts } if hosts.contains(&"registry.npmjs.org".into())),
                "BN1 explicit package registry"
            );
        }
    }
}
