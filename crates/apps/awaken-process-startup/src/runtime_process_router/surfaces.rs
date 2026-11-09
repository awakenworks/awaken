// @formal evidence assembly_role.process_role_projects_exact_startup_authority
//! Preserve canonical IAM, audit, Work admission and private-listener boundaries.

use awaken_config_service::ManagementAuditPlane;
use awaken_control::{HttpPrivateWorkspaceContext, ManagementAuthz, RemoteManagementAuthz};
use axum::Router;
use std::sync::Arc;

pub(super) struct ProtectedSurfaces {
    pub(super) data: Router,
    pub(super) private: Router,
}

pub(super) fn protect(
    coordinator: awaken_coordinator::CoordinatorComponent,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    audit_plane: ManagementAuditPlane,
    managed_rate_limiter: Arc<dyn awaken_tenancy::org_admission::OrgRequestAdmission>,
    private_context: Option<Arc<HttpPrivateWorkspaceContext>>,
    agent_governance: Option<awaken_control::AgentGovernanceConfig>,
) -> ProtectedSurfaces {
    let private_reads = private_context.map(|context| {
        // The private ingress performs its own current human authentication and
        // native PDP call before it invokes this same Managed handler graph.
        // Retain the ordinary Managed response and Org admission boundaries.
        let sessions = awaken_protocol_managed::with_managed_response_context(
            awaken_control::protect_management_router(
                coordinator.managed_router.clone(),
                audit_plane.clone(),
                None,
                None,
                Some(managed_rate_limiter.clone()),
            ),
        );
        awaken_control::private_assistant_router(
            sessions,
            remote_iam
                .clone()
                .expect("private ingress preflight requires remote IAM"),
            context,
        )
    });
    let coordinator_data = awaken_control::protect_managed_agent_definitions(
        awaken_control::protect_runtime_protocol_routers(
            coordinator.router,
            coordinator.application_router,
            iam.clone(),
            remote_iam.clone(),
        ),
        remote_iam.clone(),
        agent_governance.clone(),
    );
    let coordinator_management = awaken_protocol_managed::with_managed_response_context(
        awaken_control::protect_managed_agent_definitions(
            awaken_control::protect_management_router(
                coordinator.management_router,
                audit_plane.clone(),
                iam.clone(),
                remote_iam.clone(),
                Some(managed_rate_limiter.clone()),
            ),
            remote_iam.clone(),
            agent_governance.clone(),
        )
        .layer(axum::middleware::from_fn_with_state(
            coordinator.work_session_access.clone(),
            awaken_protocol_managed::work_session_guard,
        )),
    );
    let coordinator_managed = awaken_protocol_managed::with_managed_response_context(
        awaken_control::protect_managed_agent_definitions(
            awaken_control::protect_management_router(
                coordinator.managed_router,
                audit_plane,
                iam,
                remote_iam.clone(),
                Some(managed_rate_limiter.clone()),
            ),
            remote_iam.clone(),
            agent_governance.clone(),
        )
        .layer(axum::middleware::from_fn_with_state(
            coordinator.work_session_access,
            awaken_protocol_managed::work_session_guard,
        )),
    );
    let data = coordinator_data
        .merge(coordinator_managed)
        .merge(coordinator_management);
    let data = match private_reads {
        Some(reads) => data.merge(reads),
        None => data,
    };
    ProtectedSurfaces {
        data,
        private: coordinator.private_router,
    }
}
