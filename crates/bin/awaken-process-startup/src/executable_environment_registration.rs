// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: This source implements the repository-controlled snapshot product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Role-specific startup of the executable Environment boundary.

use std::sync::Arc;

use awaken_executable_environment_catalog::{
    ExecutableEnvironmentCatalog, HttpExecutableEnvironmentRegistrar,
    LocalExecutableEnvironmentRegistrar, PostgresExecutableEnvironmentRegistrar,
    executable_environment_registration_router_with_authenticator,
};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;
use axum::Router;

use crate::PostgresSchemaMode;
use crate::config::{ResolvedDeployment, Role};

pub(crate) struct ExecutableEnvironmentWiring {
    pub(crate) catalog: Arc<ExecutableEnvironmentCatalog>,
    pub(crate) registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    pub(crate) projection_refresher: Option<Arc<PostgresExecutableEnvironmentRegistrar>>,
    pub(crate) private_router: Router,
    pub(crate) image_builds:
        Option<Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>,
}

impl ExecutableEnvironmentWiring {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn local(
        work: impl Into<awaken_environment_application::execution::WorkQueue>,
    ) -> Result<Self, String> {
        Self::local_with_image_builds(work, None)
    }

    fn local_with_image_builds(
        work: impl Into<awaken_environment_application::execution::WorkQueue>,
        image_builds: Option<Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>,
    ) -> Result<Self, String> {
        let work = work.into();
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar = coordinator_registrar(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work.lifecycle(),
            image_builds.as_ref(),
        );
        Ok(Self {
            catalog,
            registrar,
            projection_refresher: None,
            private_router: Router::new(),
            image_builds,
        })
    }

    pub(crate) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        Ok(Self {
            catalog: Arc::new(ExecutableEnvironmentCatalog::new()),
            registrar: control_registrar(deployment)?,
            projection_refresher: None,
            private_router: Router::new(),
            image_builds: None,
        })
    }

    async fn coordinator(
        deployment: &ResolvedDeployment,
        schema: PostgresSchemaMode,
        work: awaken_environment_application::execution::WorkQueue,
        service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
        postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, String> {
        let authenticator = deployment
            .executable_agent_registration
            .coordinator_authenticator()?;
        let database_url = deployment.runtime.database_url.as_deref().ok_or_else(|| {
            "Coordinator requires runtime_database_url for executable Environment registration"
                .to_owned()
        })?;
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let durable = Arc::new(
            match schema {
                PostgresSchemaMode::Migrate => {
                    PostgresExecutableEnvironmentRegistrar::with_pool(
                        postgres_pools.get(database_url).await?,
                        catalog.clone(),
                    )
                    .await
                }
                PostgresSchemaMode::Verify => {
                    PostgresExecutableEnvironmentRegistrar::with_existing_pool(
                        postgres_pools.get(database_url).await?,
                        catalog.clone(),
                    )
                    .await
                }
            }
            .map_err(|error| error.to_string())?,
        );
        let image_builds =
            open_image_builds(deployment, schema, service_lifecycle, postgres_pools, clock).await?;
        let registrar =
            coordinator_registrar(durable.clone(), work.lifecycle(), image_builds.as_ref());
        let private_router = executable_environment_registration_router_with_authenticator(
            registrar.clone(),
            authenticator,
        );
        Ok(Self {
            catalog,
            registrar,
            projection_refresher: Some(durable),
            private_router,
            image_builds,
        })
    }
}

pub(crate) async fn for_runtime_role(
    role: Role,
    deployment: &ResolvedDeployment,
    schema: PostgresSchemaMode,
    work: awaken_environment_application::execution::WorkQueue,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<ExecutableEnvironmentWiring, String> {
    match role {
        Role::AllInOne => {
            let image_builds =
                open_image_builds(deployment, schema, service_lifecycle, postgres_pools, clock)
                    .await?;
            ExecutableEnvironmentWiring::local_with_image_builds(work, image_builds)
        }
        Role::Coordinator => {
            ExecutableEnvironmentWiring::coordinator(
                deployment,
                schema,
                work,
                service_lifecycle,
                postgres_pools,
                clock,
            )
            .await
        }
        Role::Control | Role::Worker => {
            unreachable!("runtime process accepts only AllInOne or Coordinator")
        }
    }
}

fn coordinator_registrar(
    registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    work: Arc<dyn awaken_executable_environment_contract::EnvironmentWorkLifecycle>,
    image_builds: Option<&Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>,
) -> Arc<dyn ExecutableEnvironmentRegistrar> {
    let registrar = match image_builds {
        Some(builds) => Arc::new(
            awaken_environment_image_build::BuildAwareExecutableEnvironmentRegistrar::new(
                registrar,
                builds.clone(),
            ),
        ) as Arc<dyn ExecutableEnvironmentRegistrar>,
        None => registrar,
    };
    Arc::new(
        awaken_environment_application::execution::CoordinatorEnvironmentRegistrar::new(
            registrar, work,
        ),
    )
}

/// The Coordinator's Environment image builder: the deployment's builder
/// (`SandboxSettings::package_image_builder_for`) when this build links its
/// engine, none otherwise. Builder Pods are created directly, not brokered.
async fn package_image_provisioner(
    sandbox: &awaken_deployment_config::SandboxSettings,
    tier: awaken_deployment_config::SandboxTier,
) -> Result<Option<Arc<dyn awaken_container_contract::PackageImageProvisioner>>, String> {
    use awaken_deployment_config::PackageImageBuilder;
    let Some(builder) = sandbox.package_image_builder_for(tier) else {
        return Ok(None);
    };
    Ok(match builder {
        #[cfg(feature = "container-docker")]
        PackageImageBuilder::Docker => Some(Arc::new(
            awaken_package_image::docker_package_image_provisioner(
                &sandbox.package_image_build_settings(),
            )?,
        )),
        #[cfg(feature = "container-podman")]
        PackageImageBuilder::Podman => Some(Arc::new(
            awaken_package_image::podman_package_image_provisioner(
                &sandbox.package_image_build_settings(),
            )?,
        )),
        #[cfg(feature = "container-k8s")]
        PackageImageBuilder::Kubernetes => Some(Arc::new(
            awaken_package_image::kubernetes_package_image_provisioner(
                &sandbox.package_image_build_settings(),
                None,
            )
            .await?,
        )),
        #[cfg(not(feature = "container-docker"))]
        PackageImageBuilder::Docker => None,
        #[cfg(not(feature = "container-podman"))]
        PackageImageBuilder::Podman => None,
        #[cfg(not(feature = "container-k8s"))]
        PackageImageBuilder::Kubernetes => None,
    })
}

async fn open_image_builds(
    deployment: &ResolvedDeployment,
    schema: PostgresSchemaMode,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<Option<Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>, String> {
    let Some(provisioner) =
        package_image_provisioner(&deployment.runtime.sandbox, deployment.runtime.sandbox_tier)
            .await?
    else {
        return Ok(None);
    };
    let builder =
        awaken_environment_package_image_builder::package_environment_image_builder(provisioner);
    let base_image = deployment
        .runtime
        .container_image
        .clone()
        .ok_or_else(|| "Environment image builder requires container_image".to_owned())?;
    let store = match &deployment.coordinator.sessions {
        awaken_control::StoreBackend::Sqlite(_) => {
            awaken_environment_image_build::open_sqlite_environment_image_build_store(
                deployment.data_dir.join("environment_images.db"),
            )
            .await
            .map_err(|error| error.to_string())?
        }
        awaken_control::StoreBackend::Postgres(database_url) => match schema {
            PostgresSchemaMode::Migrate => {
                awaken_environment_image_build::with_postgres_pool_environment_image_build_store(
                    postgres_pools.get(database_url).await?,
                )
                .await
            }
            PostgresSchemaMode::Verify => {
                awaken_environment_image_build::with_existing_postgres_pool_environment_image_build_store(
                    postgres_pools.get(database_url).await?,
                )
                .await
            }
        }
        .map_err(|error| error.to_string())?,
    };
    let coordinator = Arc::new(
        awaken_environment_image_build::EnvironmentImageBuildCoordinator::new(
            store,
            builder,
            base_image,
            awaken_environment_image_build::EnvironmentImageBuildPolicy::default(),
            clock,
        )
        .map_err(|error| error.to_string())?,
    );
    let worker = coordinator.clone();
    let owner = deployment.runtime.dispatch_owner.clone();
    service_lifecycle.spawn(
        "coordinator-environment-image-build",
        move |cancel| async move { worker.run_worker(owner, cancel).await },
    );
    Ok(Some(coordinator))
}

pub(crate) fn control_registrar(
    deployment: &ResolvedDeployment,
) -> Result<Arc<dyn ExecutableEnvironmentRegistrar>, String> {
    let (coordinator_url, token_source) = deployment
        .executable_agent_registration
        .control_credentials()?;
    Ok(Arc::new(
        HttpExecutableEnvironmentRegistrar::with_token_source(coordinator_url, token_source)
            .map_err(|error| error.to_string())?,
    ))
}

pub(crate) async fn migrate(
    deployment: &ResolvedDeployment,
    postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
) -> Result<(), String> {
    let Some(database_url) = deployment.runtime.database_url.as_deref() else {
        return Ok(());
    };
    PostgresExecutableEnvironmentRegistrar::with_pool(
        postgres_pools.get(database_url).await?,
        Arc::new(ExecutableEnvironmentCatalog::new()),
    )
    .await
    .map(drop)
    .map_err(|error| format!("migrate executable Environment catalog: {error}"))?;
    awaken_environment_image_build::with_postgres_pool_environment_image_build_store(
        postgres_pools.get(database_url).await?,
    )
    .await
    .map(drop)
    .map_err(|error| format!("migrate Environment image-build jobs: {error}"))
}

#[cfg(test)]
pub(crate) fn local_test_wiring() -> ExecutableEnvironmentWiring {
    ExecutableEnvironmentWiring::local(Arc::new(awaken_work_store::InMemoryWorkQueue::new(
        awaken_time::system_clock(),
    )))
    .expect("configure test executable Environment wiring")
}

#[cfg(test)]
mod startup_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use awaken_environment_contract::{
        EnvItem, EnvironmentConfig, EnvironmentPackages, EnvironmentRevision,
    };
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistration;
    use awaken_executable_environment_contract::{
        EnvironmentImageBuildDemand, EnvironmentImageBuildError, EnvironmentImageBuildStore,
        EnvironmentImageBuilder,
    };

    struct NeverRunBuilder;

    #[async_trait]
    impl EnvironmentImageBuilder for NeverRunBuilder {
        async fn build(
            &self,
            _demand: &EnvironmentImageBuildDemand,
        ) -> Result<String, EnvironmentImageBuildError> {
            panic!("composition test does not run the asynchronous build worker")
        }

        async fn available(
            &self,
            _demand: &EnvironmentImageBuildDemand,
            _image: &str,
        ) -> Result<bool, EnvironmentImageBuildError> {
            Ok(false)
        }
    }

    fn packaged_registration(id: &str) -> ExecutableEnvironmentRegistration {
        ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: id.into(),
                revision: EnvironmentRevision(1),
                name: id.into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: EnvironmentPackages {
                        npm: vec!["tsx@4".into()],
                        ..Default::default()
                    },
                },
                sandbox_policy: None,
                archived_at: None,
                created_at: Some("2026-02-03T04:05:06.007Z".into()),
                updated_at: Some("2026-02-03T04:05:06.007Z".into()),
            },
            None,
        )
    }

    #[test]
    fn missing_wiring_never_allocates_a_parallel_catalog_or_queue() {
        // Cause/effect decision table: E1 explicit local wiring -> catalog shares
        // the caller's WorkQueue; E2 explicit distributed wiring -> durable
        // catalog/queue; E3 missing wiring -> startup failure. Positive E1/E2
        // are covered by role process; this test owns E3 at the actual entry
        // point, which now returns an error rather than panicking in a helper.
        awaken_service_lifecycle::run_composed_async_test(|| async {
            let stores = crate::local_process_stores::in_memory_process_stores().await;
            let mut process = crate::exact_host_model::local_test_process_options(
                &stores,
                std::sync::Arc::new(crate::colocated_worker::WorkdirExecution),
            );
            process.executable_environment_wiring = None;
            let result = crate::prepare_runtime_routers(
                stores,
                None,
                None,
                None,
                crate::PublicationModelSupply::PublishedProviders,
                process,
                None,
            )
            .await;
            assert!(
                result
                    .err()
                    .expect("E3 startup error")
                    .contains("executable Environment wiring")
            );
        });
    }

    #[tokio::test]
    async fn one_composition_path_makes_image_builds_strictly_optional() {
        // FMECA: F1 AllInOne and Coordinator decorate registrars in different
        // orders (S8/O3/D5, RPN120) -> only one role persists build demand; F2
        // missing container-image provisioner still creates demand (S7/O4/D4,
        // RPN112) -> package Environments fail in otherwise valid local mode.
        // Mitigation: both roles call `coordinator_registrar`; `Some(builds)` is
        // the sole BuildAware switch and `None` has no hidden demand source.
        //
        // Cause/effect decision table:
        // | Rule | image coordinator | packaged registration | effects |
        // | C1 | None | yes | catalog registers; no image service/demand |
        // | C2 | Some | yes | same catalog path + exactly one Pending demand |
        let without = super::ExecutableEnvironmentWiring::local(Arc::new(
            awaken_work_store::InMemoryWorkQueue::new(awaken_time::system_clock()),
        ))
        .unwrap();
        assert!(without.image_builds.is_none(), "C1");
        without
            .registrar
            .register(packaged_registration("without-builder"))
            .await
            .unwrap();
        assert!(without.catalog.current("without-builder").is_some(), "C1");

        let store =
            Arc::new(awaken_environment_image_build::InMemoryEnvironmentImageBuildStore::new());
        let builds = Arc::new(
            awaken_environment_image_build::EnvironmentImageBuildCoordinator::new(
                store.clone(),
                Arc::new(NeverRunBuilder),
                "registry/awaken@sha256:base",
                awaken_environment_image_build::EnvironmentImageBuildPolicy::default(),
                awaken_time::system_clock(),
            )
            .unwrap(),
        );
        let with = super::ExecutableEnvironmentWiring::local_with_image_builds(
            Arc::new(awaken_work_store::InMemoryWorkQueue::new(
                awaken_time::system_clock(),
            )),
            Some(builds),
        )
        .unwrap();
        let registration = packaged_registration("with-builder");
        let demand = EnvironmentImageBuildDemand::from_registration(
            &registration,
            "registry/awaken@sha256:base",
        )
        .unwrap();
        with.registrar.register(registration).await.unwrap();
        assert!(with.catalog.current("with-builder").is_some(), "C2");
        assert!(store.get(&demand.build_key).await.unwrap().is_some(), "C2");
    }
}
