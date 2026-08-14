//! Canonical Control application component assembly.
//!
//! Process composition supplies concrete adapters through Control-owned ports.
//! Both a standalone Control process and an AllInOne process call this one
//! builder; neither process is allowed to reconstruct the Config Service,
//! management assistant, reconciler, vault state, or Control router itself.

use std::sync::Arc;
use std::time::Duration;

use awaken_admin_assistant::ResourceInventory;
use awaken_admin_config_api::{BrokeredCatalogDiscovery, CredentialProbe, ModelCatalogDiscovery};
use awaken_agent_config::{ModelSelection, ScopedConfigRegistry};
use awaken_config_resolver::{AgentInputBindingRepository, InferenceProfileStore, WebhookStore};
use awaken_config_service::{
    ConfigPlane, ConfigService, ConfigServiceReconciler, ManagementAuditPlane,
    ModelPublicationResolver, PluginPublicationResolver, PublicationBindingReconciler,
    RESERVED_ADMIN_SCOPE, RuntimeCapabilitySource, ScopedToolCatalog, ToolCatalogSource,
};
use awaken_credential_vault::SecretStore;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_data_subject_application::{
    DataSubjectApplication, DataSubjectRepo, ErasureJobRepo, ErasureTarget, RepoDataSubjectResolver,
};
use awaken_environment_contract::EnvironmentAuthor;
use awaken_executable_agent_contract::ExecutableAgentRegistrar;
use awaken_model_catalog::repo::CatalogRepo;
use awaken_protocol_managed::{ManagedAgentRepository, VaultState};
use awaken_runtime_contract::capability::PluginCapability;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;
use awaken_session_contract::McpProbe;
use axum::Router;

use crate::{
    CatalogCapabilityReader, ConfigPlaneManagedAgentRepository, ConfigServiceDraftStore,
    ConfigServiceDraftValidator, ControlRouterInput, ManagementAuthz, RemoteManagementAuthz,
    control_router, seed_admin_assistant,
};

const CREDENTIAL_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(60);

/// Hosted tenant Workspaces are selected at authenticated request time. A
/// process-local coordinate cannot safely recover or refresh their reserved
/// Assistant registrations, so the fixed-workspace supervisor has no Agent
/// work in that composition. Request-time ensure remains the sole publisher.
struct RequestScopedPublicationReconciler;

#[async_trait::async_trait]
impl PublicationBindingReconciler for RequestScopedPublicationReconciler {
    async fn reconcile(&self) -> Result<usize, String> {
        Ok(0)
    }

    async fn reconcile_all(&self) -> Result<usize, String> {
        Ok(0)
    }
}

/// Control-owned ports and immutable composition facts.
///
/// Execution repositories, dispatch queues, Runtime commit stores, resource
/// content databases, and Worker hosts are deliberately absent. Cross-domain
/// information enters through narrow read or command ports.
pub struct ControlDependencies {
    /// Process-owned cancellation/readiness boundary for every recurring
    /// Control task. The component registers tasks but never owns shutdown.
    pub service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    pub execution_workspace: String,
    pub data_subject_org: String,
    pub enrollment_signing_key: [u8; 32],
    pub catalog: Arc<dyn CatalogRepo>,
    pub credentials: Arc<dyn CredentialRepo>,
    pub secrets: Arc<dyn SecretStore>,
    pub profiles: Arc<dyn InferenceProfileStore>,
    pub webhook_store: Arc<dyn WebhookStore>,
    pub resource_store: Arc<dyn AgentInputBindingRepository>,
    pub config_store: Arc<dyn ScopedConfigRegistry>,
    pub executable_agent_registrar: Arc<dyn ExecutableAgentRegistrar>,
    /// Optional local lifecycle command edge. Present only in AllInOne
    /// composition; its execution-owned implementation stays outside Control.
    pub agent_archive_cascade: Option<Arc<dyn awaken_deployment_contract::AgentArchiveCascade>>,
    pub model_publication_resolver: Arc<dyn ModelPublicationResolver>,
    pub plugin_publication_resolvers: Vec<Arc<dyn PluginPublicationResolver>>,
    pub credential_probe: Arc<dyn CredentialProbe>,
    pub model_discovery: Arc<dyn ModelCatalogDiscovery>,
    pub brokered_catalog: Option<Arc<dyn BrokeredCatalogDiscovery>>,
    pub model_supply: awaken_admin_config_api::ModelSupplyCapabilityView,
    /// Whether the browser-serving origin can reach the canonical Managed
    /// runtime/resources, either through a local mount or hosted routing.
    pub managed_runtime_available: bool,
    pub mcp_probe: Option<Arc<dyn McpProbe>>,
    pub assistant_model_selection: Option<ModelSelection>,
    pub global_tools: Vec<ToolDescriptor>,
    pub platform_plugins: Vec<PluginCapability>,
    pub assistant_plugins: Vec<PluginCapability>,
    pub runtimes: Arc<dyn RuntimeCapabilitySource>,
    pub resource_inventory: Option<Arc<dyn ResourceInventory>>,
    pub environment_author: Arc<dyn EnvironmentAuthor>,
    /// The canonical Environment application is also the Environment half of
    /// the shared static-registration recovery supervisor.
    pub environment_application: Arc<awaken_environment_application::EnvironmentApplication>,
    /// Control-owned Environment definition and policy authoring surface.
    pub environment_router: Router,
    /// Optional Cloud-only Tunnel application. The public wire stays in the
    /// Managed ACL and is merged here before the canonical management IAM and
    /// audit edge; an absent port means no Tunnel routes are mounted.
    pub managed_tunnel_application:
        Option<Arc<dyn awaken_protocol_managed::ManagedTunnelApplication>>,
    pub managed_request_limiter: Option<Arc<dyn awaken_protocol_managed::ManagedRequestLimiter>>,
    pub credential_envelope_issuer:
        Option<Arc<dyn awaken_credential_contract::CredentialEnvelopeIssuer>>,
    pub data_subjects: Arc<dyn DataSubjectRepo>,
    pub erasure_jobs: Arc<dyn ErasureJobRepo>,
    pub coordinator_content_eraser: Arc<dyn awaken_runtime_contract::ContentEraser>,
    pub resource_content_eraser: Option<Arc<dyn awaken_runtime_contract::ContentEraser>>,
    pub content_capture_ceiling: awaken_runtime_contract::ContentCapture,
    pub iam: Option<Arc<ManagementAuthz>>,
    pub local_browser_auth: Option<awaken_iam_host::LocalBrowserAuth>,
    pub remote_iam: Option<Arc<RemoteManagementAuthz>>,
}

/// The complete Control application component exported to a process assembly.
///
/// The router is the public Control surface. The remaining handles are explicit
/// local adapters used only when AllInOne replaces network boundaries with
/// in-process calls.
pub struct ControlComponent {
    pub router: Router,
    pub management_audit: ManagementAuditPlane,
    pub publication_reconciler: Arc<dyn PublicationBindingReconciler>,
    pub registration_supervisor: Arc<crate::StaticRegistrationSupervisor>,
    pub admin_tools: Vec<Arc<dyn RawTool>>,
    pub vault_state: Arc<VaultState>,
    /// The same Control-owned consent source exposed locally to AllInOne. A
    /// split Coordinator consumes its authenticated HTTP projection instead.
    pub data_subject_consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
}

/// Build the one authoritative Control application component.
pub async fn build_control_component(dependencies: ControlDependencies) -> ControlComponent {
    let ControlDependencies {
        service_lifecycle,
        execution_workspace,
        data_subject_org,
        enrollment_signing_key,
        catalog,
        credentials,
        secrets,
        profiles,
        webhook_store,
        resource_store,
        config_store,
        executable_agent_registrar,
        agent_archive_cascade,
        model_publication_resolver,
        plugin_publication_resolvers,
        credential_probe,
        model_discovery,
        brokered_catalog,
        model_supply,
        managed_runtime_available,
        mcp_probe,
        assistant_model_selection,
        global_tools,
        platform_plugins,
        assistant_plugins,
        runtimes,
        resource_inventory,
        environment_author,
        environment_application,
        environment_router,
        managed_tunnel_application,
        managed_request_limiter,
        credential_envelope_issuer,
        data_subjects,
        erasure_jobs,
        coordinator_content_eraser,
        resource_content_eraser,
        content_capture_ceiling,
        iam,
        local_browser_auth,
        remote_iam,
    } = dependencies;
    let request_scoped_execution_workspace = remote_iam.is_some();

    recover_and_supervise_credentials(secrets.clone(), credentials.clone(), &service_lifecycle)
        .await;
    recover_and_supervise_webhooks(secrets.clone(), webhook_store.clone(), &service_lifecycle)
        .await;

    let mut vault_state = VaultState::new(secrets.clone(), credentials.clone());
    if let Some(probe) = mcp_probe {
        vault_state = vault_state.with_probe(probe);
    }
    if let Some(issuer) = credential_envelope_issuer {
        vault_state = vault_state.with_envelope_issuer(issuer);
    }
    let vault_state = Arc::new(vault_state);

    let tool_catalog: Arc<dyn ToolCatalogSource> = Arc::new(ScopedToolCatalog::new(
        global_tools.clone(),
        RESERVED_ADMIN_SCOPE,
        awaken_admin_assistant::admin_tool_descriptors(),
    ));
    let mut config_service =
        ConfigService::new(model_publication_resolver, executable_agent_registrar)
            .with_credential_reference_validator(Arc::new(crate::CredentialRevisionValidator::new(
                credentials.clone(),
            )))
            .with_resources(resource_store.clone());
    for resolver in plugin_publication_resolvers {
        config_service = config_service.with_plugin_publication_resolver(resolver);
    }
    let config_service = Arc::new(config_service);
    let config_plane = ConfigPlane::new(config_service, config_store, tool_catalog);
    if !request_scoped_execution_workspace
        && let Some(selection) = assistant_model_selection.clone()
        && let Err(error) =
            seed_admin_assistant(&config_plane, &execution_workspace, selection).await
    {
        eprintln!("admin assistant not seeded (configure a model, then republish): {error}");
    }

    let publication_reconciler: Arc<dyn PublicationBindingReconciler> =
        if request_scoped_execution_workspace {
            Arc::new(RequestScopedPublicationReconciler)
        } else {
            Arc::new(ConfigServiceReconciler::new(
                config_plane.clone(),
                RESERVED_ADMIN_SCOPE,
                execution_workspace.clone(),
                vec![awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID.to_string()],
            ))
        };
    let registration_supervisor = crate::StaticRegistrationSupervisor::start(
        publication_reconciler.clone(),
        environment_application,
        &service_lifecycle,
    );
    let capability_reader = Arc::new(if request_scoped_execution_workspace {
        CatalogCapabilityReader::request_scoped(
            catalog.clone(),
            &global_tools,
            &assistant_plugins,
            config_plane.clone(),
            resource_inventory,
        )
    } else {
        CatalogCapabilityReader::new(
            catalog.clone(),
            &global_tools,
            &assistant_plugins,
            config_plane.clone(),
            execution_workspace.clone(),
            resource_inventory,
        )
    });
    let draft_store = Arc::new(if request_scoped_execution_workspace {
        ConfigServiceDraftStore::request_scoped(config_plane.clone(), resource_store.clone())
    } else {
        ConfigServiceDraftStore::new(
            config_plane.clone(),
            execution_workspace.clone(),
            resource_store.clone(),
        )
    });
    let draft_reconciler = draft_store.clone();
    service_lifecycle.spawn(
        "control-agent-resource-binding-reconciliation",
        move |cancel| async move {
            draft_reconciler
                .run_resource_effect_reconciliation(cancel)
                .await
        },
    );
    let admin_tools = awaken_admin_assistant::admin_tools(
        capability_reader,
        Arc::new(if request_scoped_execution_workspace {
            ConfigServiceDraftValidator::request_scoped(config_plane.clone())
        } else {
            ConfigServiceDraftValidator::new(config_plane.clone(), execution_workspace.clone())
        }),
        draft_store,
        environment_author,
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    let agent_repository: Arc<dyn ManagedAgentRepository> =
        Arc::new(if request_scoped_execution_workspace {
            ConfigPlaneManagedAgentRepository::request_scoped(config_plane.clone())
        } else {
            ConfigPlaneManagedAgentRepository::new(
                config_plane.clone(),
                execution_workspace.clone(),
            )
        });
    let data_subject_application = Arc::new(
        DataSubjectApplication::new(data_subjects.clone(), enrollment_signing_key.to_vec())
            .expect("a derived 32-byte enrollment signing key is valid"),
    );
    let resolver = RepoDataSubjectResolver::new(data_subjects, erasure_jobs)
        .with_target(ErasureTarget::Coordinator, coordinator_content_eraser);
    let resolver = match resource_content_eraser {
        Some(eraser) => resolver.with_target(ErasureTarget::Resources, eraser),
        None => resolver,
    };
    let resolver = Arc::new(resolver);
    let data_subject_consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource> =
        resolver.clone();
    let data_subject_resolver: Arc<dyn awaken_runtime_contract::DataSubjectResolver> = resolver;
    let router = control_router(ControlRouterInput {
        catalog,
        credentials,
        secrets,
        profiles,
        webhook_store,
        resource_store,
        probe: credential_probe,
        model_discovery,
        brokered_catalog,
        model_supply,
        managed_runtime_available,
        vault_state: vault_state.clone(),
        agent_repository: agent_repository.clone(),
        agent_archive_cascade,
        plane: config_plane.clone(),
        assistant_model_selection,
        global_tools,
        plugins: platform_plugins,
        runtimes,
        platform_workspace: execution_workspace,
        data_subject_application: data_subject_application.clone(),
        data_subject_org: data_subject_org.clone(),
        environment_router,
        managed_tunnel_application,
        managed_request_limiter,
        iam,
        local_browser_auth,
        remote_iam,
    })
    .merge(crate::consent_router(
        data_subject_application.clone(),
        data_subject_org.clone(),
        content_capture_ceiling,
    ))
    .merge(crate::erasure_router(
        data_subject_application,
        data_subject_resolver,
        data_subject_org,
    ));

    ControlComponent {
        router,
        management_audit: config_plane.management_audit_plane(),
        publication_reconciler,
        registration_supervisor,
        admin_tools,
        vault_state,
        data_subject_consent,
    }
}

async fn recover_and_supervise_credentials(
    secrets: Arc<dyn SecretStore>,
    credentials: Arc<dyn CredentialRepo>,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) {
    if let Err(error) = awaken_credential_vault::repo::recover_credential_mutations(
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    {
        eprintln!("credential mutation recovery failed: {error}");
    }
    report_credential_inventory(secrets.as_ref(), credentials.as_ref()).await;

    service_lifecycle.spawn(
        "control-credential-reconciliation",
        move |cancel| async move {
            let mut interval = tokio::time::interval(CREDENTIAL_RECONCILIATION_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => {}
                }
                if let Err(error) = awaken_credential_vault::repo::recover_credential_mutations(
                    secrets.as_ref(),
                    credentials.as_ref(),
                )
                .await
                {
                    eprintln!("credential mutation reconciliation failed: {error}");
                }
                report_credential_inventory(secrets.as_ref(), credentials.as_ref()).await;
            }
            Ok(())
        },
    );
}

async fn report_credential_inventory(secrets: &dyn SecretStore, credentials: &dyn CredentialRepo) {
    match awaken_credential_vault::repo::reconcile_credential_inventory(secrets, credentials).await
    {
        Ok(report) if !report.missing_material.is_empty() => eprintln!(
            "credential inventory is missing referenced material: {:?}",
            report.missing_material
        ),
        Err(error) => eprintln!("credential inventory reconciliation failed: {error}"),
        _ => {}
    }
}

async fn recover_and_supervise_webhooks(
    secrets: Arc<dyn SecretStore>,
    webhooks: Arc<dyn WebhookStore>,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) {
    if let Err(error) =
        awaken_webhook_managed::recover_webhook_mutations(webhooks.as_ref(), secrets.as_ref()).await
    {
        eprintln!("webhook material mutation recovery failed: {error}");
    }
    report_webhook_inventory(webhooks.as_ref(), secrets.as_ref()).await;

    service_lifecycle.spawn("control-webhook-reconciliation", move |cancel| async move {
        let mut interval = tokio::time::interval(CREDENTIAL_RECONCILIATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            if let Err(error) = awaken_webhook_managed::recover_webhook_mutations(
                webhooks.as_ref(),
                secrets.as_ref(),
            )
            .await
            {
                eprintln!("webhook material mutation reconciliation failed: {error}");
            }
            report_webhook_inventory(webhooks.as_ref(), secrets.as_ref()).await;
        }
        Ok(())
    });
}

async fn report_webhook_inventory(webhooks: &dyn WebhookStore, secrets: &dyn SecretStore) {
    match awaken_webhook_managed::reconcile_webhook_inventory(webhooks, secrets).await {
        Ok(report) if !report.missing_material.is_empty() => eprintln!(
            "webhook inventory is missing referenced material: {:?}",
            report.missing_material
        ),
        Err(error) => eprintln!("webhook inventory reconciliation failed: {error}"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hosted_registration_recovery_never_replays_a_process_local_assistant() {
        // Cause/effect decision rule: hosted IAM selects Workspace per request,
        // so there is no fixed execution Workspace (C1). Every startup,
        // freshness, and recovery operation on the hosted reconciler is an
        // intentional no-op (E1); an old local __admin_assistant publication
        // therefore cannot poison hosted readiness by being replayed. The local
        // fixed-workspace branch remains owned by ConfigServiceReconciler.
        let reconciler = RequestScopedPublicationReconciler;
        assert_eq!(reconciler.reconcile().await.unwrap(), 0, "C1 -> E1");
        assert_eq!(reconciler.reconcile_all().await.unwrap(), 0, "C1 -> E1");
        assert_eq!(
            reconciler.recover_registrations().await.unwrap(),
            0,
            "C1 -> E1"
        );
    }
}
