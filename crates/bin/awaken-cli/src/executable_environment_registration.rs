//! Role-specific startup of the executable Environment boundary.

use std::sync::Arc;

use awaken_executable_environment_catalog::{
    ExecutableEnvironmentCatalog, HttpExecutableEnvironmentRegistrar,
    LocalExecutableEnvironmentRegistrar, PostgresExecutableEnvironmentRegistrar,
    executable_environment_registration_router_with_authenticator,
};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;
use awaken_session_contract::work_queue::WorkQueue;
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
    pub(crate) fn local(work: Arc<dyn WorkQueue>) -> Result<Self, String> {
        Self::local_with_image_builds(work, None)
    }

    fn local_with_image_builds(
        work: Arc<dyn WorkQueue>,
        image_builds: Option<Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>,
    ) -> Result<Self, String> {
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar = coordinator_registrar(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work,
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
        work: Arc<dyn WorkQueue>,
        service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
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
                    PostgresExecutableEnvironmentRegistrar::connect(database_url, catalog.clone())
                        .await
                }
                PostgresSchemaMode::Verify => {
                    PostgresExecutableEnvironmentRegistrar::connect_existing(
                        database_url,
                        catalog.clone(),
                    )
                    .await
                }
            }
            .map_err(|error| error.to_string())?,
        );
        let image_builds = open_image_builds(deployment, schema, service_lifecycle).await?;
        let registrar = coordinator_registrar(durable.clone(), work, image_builds.as_ref());
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
    work: Arc<dyn WorkQueue>,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) -> Result<ExecutableEnvironmentWiring, String> {
    match role {
        Role::AllInOne => {
            let image_builds = open_image_builds(deployment, schema, service_lifecycle).await?;
            ExecutableEnvironmentWiring::local_with_image_builds(work, image_builds)
        }
        Role::Coordinator => {
            ExecutableEnvironmentWiring::coordinator(deployment, schema, work, service_lifecycle)
                .await
        }
        Role::Control | Role::Worker => {
            unreachable!("runtime process accepts only AllInOne or Coordinator")
        }
    }
}

fn coordinator_registrar(
    registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    work: Arc<dyn WorkQueue>,
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
        awaken_environment_execution_application::CoordinatorEnvironmentRegistrar::new(
            registrar, work,
        ),
    )
}

async fn open_image_builds(
    deployment: &ResolvedDeployment,
    schema: PostgresSchemaMode,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) -> Result<Option<Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>, String> {
    let Some(provisioner) =
        awaken_runtime_host::package_image_provisioner(&deployment.runtime).await?
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
            .map_err(|error| error.to_string())?
        }
        awaken_control::StoreBackend::Postgres(database_url) => match schema {
            PostgresSchemaMode::Migrate => {
                awaken_environment_image_build::connect_postgres_environment_image_build_store(
                    database_url,
                )
                .await
            }
            PostgresSchemaMode::Verify => {
                awaken_environment_image_build::connect_existing_postgres_environment_image_build_store(
                    database_url,
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

pub(crate) async fn migrate(deployment: &ResolvedDeployment) -> Result<(), String> {
    let Some(database_url) = deployment.runtime.database_url.as_deref() else {
        return Ok(());
    };
    PostgresExecutableEnvironmentRegistrar::connect(
        database_url,
        Arc::new(ExecutableEnvironmentCatalog::new()),
    )
    .await
    .map(drop)
    .map_err(|error| format!("migrate executable Environment catalog: {error}"))?;
    awaken_environment_image_build::connect_postgres_environment_image_build_store(database_url)
        .await
        .map(drop)
        .map_err(|error| format!("migrate Environment image-build jobs: {error}"))
}

#[cfg(test)]
pub(crate) fn local_test_wiring() -> ExecutableEnvironmentWiring {
    ExecutableEnvironmentWiring::local(Arc::new(awaken_work_store::InMemoryWorkQueue::new()))
        .expect("configure test executable Environment wiring")
}

/// Require the process-selected Environment boundary. Local AllInOne remains
/// supported through an explicit [`ExecutableEnvironmentWiring::local`] value.
pub(crate) fn require_process_wiring(
    wiring: Option<ExecutableEnvironmentWiring>,
) -> ExecutableEnvironmentWiring {
    wiring.expect("runtime process requires executable Environment wiring")
}

#[cfg(test)]
mod startup_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use awaken_environment_contract::{
        EnvItem, EnvironmentConfig, EnvironmentPackages, EnvironmentRevision,
    };
    use awaken_environment_realization_contract::{
        EnvironmentImageBuildDemand, EnvironmentImageBuildError, EnvironmentImageBuildStore,
        EnvironmentImageBuilder,
    };
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistration;

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
            },
            None,
        )
    }

    #[test]
    #[should_panic(expected = "runtime process requires executable Environment wiring")]
    fn missing_wiring_never_allocates_a_parallel_catalog_or_queue() {
        // Cause/effect decision table: E1 explicit local wiring -> catalog shares
        // the caller's WorkQueue; E2 explicit distributed wiring -> durable
        // catalog/queue; E3 missing wiring -> startup failure. Positive E1/E2
        // are covered by role process; this test owns E3.
        let _ = super::require_process_wiring(None);
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
            awaken_work_store::InMemoryWorkQueue::new(),
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
            )
            .unwrap(),
        );
        let with = super::ExecutableEnvironmentWiring::local_with_image_builds(
            Arc::new(awaken_work_store::InMemoryWorkQueue::new()),
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
