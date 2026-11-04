// @formal evidence assembly_role.local_managed_mount_requires_exact_local_worker_component
//! The launcher's owned co-located Session execution constructor.

use crate::with_platform_memory_data_plane;
use awaken_execution_composition::{
    ContainerTierPolicy, local_auxiliary_sandboxes, with_acp_from_deployment,
    with_session_environment_from_deployment, with_session_environment_from_deployment_at,
};
use awaken_process_startup::{
    ColocatedExecution, ColocatedExecutionInputs, PreparedColocatedExecution,
};
use awaken_session_executor::SessionExecutorBuilder;
use std::sync::Arc;

#[cfg(feature = "test-support")]
type HostCustomizer = Box<dyn FnOnce(SessionExecutorBuilder) -> SessionExecutorBuilder + Send>;

enum EnvironmentChoice {
    Platform,
    #[cfg(feature = "test-support")]
    Workdir,
}

/// One owned constructor, consumed once by the startup that co-locates execution.
pub struct LocalExecution {
    environment: EnvironmentChoice,
    #[cfg(feature = "test-support")]
    customize: Option<HostCustomizer>,
}

impl Default for LocalExecution {
    fn default() -> Self {
        Self {
            environment: EnvironmentChoice::Platform,
            #[cfg(feature = "test-support")]
            customize: None,
        }
    }
}

impl LocalExecution {
    /// The original Workdir-only process fixture; it starts no Hand or mounter.
    #[cfg(feature = "test-support")]
    pub fn workdir() -> Self {
        Self {
            environment: EnvironmentChoice::Workdir,
            customize: None,
        }
    }

    /// Last-mile scenario backend selection, still performed before SDK build.
    #[cfg(feature = "test-support")]
    pub fn with_host_customizer(
        mut self,
        customize: impl FnOnce(SessionExecutorBuilder) -> SessionExecutorBuilder + Send + 'static,
    ) -> Self {
        self.customize = Some(Box::new(customize));
        self
    }
}

#[async_trait::async_trait]
impl ColocatedExecution for LocalExecution {
    async fn prepare(
        self: Box<Self>,
        mut inputs: ColocatedExecutionInputs,
    ) -> Result<PreparedColocatedExecution, String> {
        let credentials = inputs
            .local_credential_roles
            .clone()
            .map(|(resolver, broker)| {
                let mut selected =
                    awaken_credential_materializer::PinnedCredentialMaterializer::from_local_roles(
                        resolver,
                        broker,
                        inputs.clock.clone(),
                    );
                if let Some(resolver) = inputs.external_credentials.clone() {
                    selected = selected.with_external_material_resolver(resolver);
                }
                selected
            });
        let mut model = crate::local_model::runtime_model_wiring(
            inputs.model_selection.clone(),
            credentials.as_ref(),
            inputs.cloud_models_enabled,
            inputs.brokered_client.as_ref(),
        );
        if let Some(selected) = inputs.inference_override.take() {
            model.materializer = Some(selected);
        }
        let (mut builder, artifact_fence) = execution_builder(&inputs, credentials.clone(), model)?;
        builder = match self.environment {
            EnvironmentChoice::Platform => install_platform(builder, &inputs, credentials).await?,
            #[cfg(feature = "test-support")]
            EnvironmentChoice::Workdir => {
                awaken_execution_composition::with_workdir_fallback(builder, &inputs.deployment)
            }
        };
        #[cfg(feature = "test-support")]
        if let Some(customize) = self.customize {
            builder = customize(builder);
        }
        let mut dependencies = awaken_session_executor::SessionExecutionDependencies::default()
            .with_resource_validator(inputs.resource_validator)
            .with_repository_binding_verifier(inputs.repository_bindings);
        if let Some(refresh) = inputs.credential_refresh {
            dependencies = dependencies.with_credential_refresh_factory(refresh);
        }
        let host = builder
            .with_session_execution(dependencies)
            .build()
            .map_err(|error| format!("construct co-located execution: {error}"))?;
        let roles = host.session_execution_roles();
        let dispatch = host.dispatch().clone();
        let dispatch_observation = host
            .coordination()
            .dispatch_observation_source()
            .map_err(|error| error.to_string())?;
        let capture_policy = host.content_capture_policy();
        Ok(PreparedColocatedExecution {
            roles,
            dispatch,
            dispatch_observation,
            capture_policy,
            artifact_fence,
            finish: Box::new(move |sessions| {
                crate::coordinator_execution(&host, &sessions).map_err(|error| error.to_string())
            }),
        })
    }
}

fn execution_builder(
    inputs: &ColocatedExecutionInputs,
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    model: crate::local_model::RuntimeModelWiring,
) -> Result<
    (
        SessionExecutorBuilder,
        Arc<awaken_coordinator::ClaimFencedArtifactPublisher>,
    ),
    String,
> {
    let artifact_fence = Arc::new(awaken_coordinator::ClaimFencedArtifactPublisher::new(
        inputs.artifact_publisher.clone(),
        inputs.runtime_authority.dispatch_store(),
        inputs.clock.clone(),
    ));
    let mut builder = SessionExecutorBuilder::for_coordinator(
        model.executor,
        model.model_ref,
        awaken_session_executor::WorkerContentAdapters {
            file_content_source: inputs.file_content.clone(),
            memory: inputs.memory_content.clone(),
        },
        inputs.memory_extractions.clone(),
        awaken_session_executor::SessionExecutionProcess::from_deployment(
            &inputs.deployment,
            inputs.local_workspace.clone(),
            inputs.clock.clone(),
            inputs.trace_file_present,
        )?,
        inputs.runtime_authority.clone(),
    )
    .with_environment_binding_sink(inputs.environment_bindings.clone())
    .with_artifact_publisher(artifact_fence.clone())
    .with_skill_bundle_source(inputs.skill_bundles.clone())
    .with_skill_catalog_application(inputs.skill_catalog.clone())
    .with_web_search_provider_registry(inputs.web_search.clone())
    .with_acp_tool_exporter(Arc::new(awaken_protocol_mcp::SessionToolExporter))
    .with_remote_attempt_executor(crate::a2a_attempt_executor(credentials.clone()))
    .with_agent_publications(inputs.agent_publications.clone())
    .with_capture_sink(inputs.capture_sink.clone())
    .with_capture_policy_sources(inputs.capture_policy.clone())
    .with_admin_tools(inputs.admin_tools.clone());
    if let Some(credentials) = credentials {
        builder = builder.with_credential_materializer(credentials);
    }
    if let Some(materializer) = model.materializer {
        builder = builder.with_inference_materializer(materializer);
    }
    Ok((builder, artifact_fence))
}

async fn install_platform(
    host: SessionExecutorBuilder,
    inputs: &ColocatedExecutionInputs,
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
) -> Result<SessionExecutorBuilder, String> {
    let deployment = &inputs.deployment;
    let namespace_hand_path = inputs.namespace_hand_path.clone();
    let memory_source = inputs.memory_source.clone();
    let memory_invalidator = inputs.memory_invalidator.clone();
    let hand_factory = awaken_tool_relay::relay_hand_executor_factory();
    let containers = ContainerTierPolicy::default();
    let host = match namespace_hand_path {
        Some(path) => {
            with_session_environment_from_deployment_at(
                host,
                deployment,
                Some(hand_factory),
                path,
                &containers,
            )
            .await?
        }
        None => {
            with_session_environment_from_deployment(
                host,
                deployment,
                Some(hand_factory),
                &containers,
            )
            .await?
        }
    };
    let host = with_acp_from_deployment(host, deployment, credentials, None)
        .with_auxiliary_sandboxes(local_auxiliary_sandboxes());
    Ok(with_platform_memory_data_plane(
        host,
        deployment.sandbox_tier,
        memory_source,
        memory_invalidator,
    ))
}
