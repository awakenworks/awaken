// @formal surface obligation=managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
// @formal reason: Mounts the access edge's Org bucket decision on the AI SDK, AG-UI and A2A entries with
// @formal reason: each protocol's own encoding; it owns no bucket state, clock or admission decision.
//! Org admission on the runtime protocol entries this composition root mounts:
//! the access edge's bucket decision with each protocol's own classification
//! and encoding. The Managed entries are admitted where Control's management
//! edge mounts them (`awaken_control::protect_management_router`).

use std::sync::Arc;

use awaken_authz_enforce::{OrgAdmissionEdge, OrgAdmissionEncoding, admit_by_org_bucket};
use awaken_tenancy::org_admission::OrgRequestAdmission;
use axum::Router;

pub(crate) const AI_SDK: OrgAdmissionEncoding = OrgAdmissionEncoding {
    classify: awaken_protocol_ai_sdk::org_admission::org_admission_class,
    scope_missing: awaken_protocol_ai_sdk::org_admission::org_admission_scope_missing,
    unavailable: awaken_protocol_ai_sdk::org_admission::org_admission_unavailable,
    rejected: awaken_protocol_ai_sdk::org_admission::org_admission_rejected,
    annotate: None,
};

pub(crate) const AG_UI: OrgAdmissionEncoding = OrgAdmissionEncoding {
    classify: awaken_protocol_ag_ui::org_admission::org_admission_class,
    scope_missing: awaken_protocol_ag_ui::org_admission::org_admission_scope_missing,
    unavailable: awaken_protocol_ag_ui::org_admission::org_admission_unavailable,
    rejected: awaken_protocol_ag_ui::org_admission::org_admission_rejected,
    annotate: None,
};

pub(crate) const A2A: OrgAdmissionEncoding = OrgAdmissionEncoding {
    classify: awaken_protocol_a2a::org_admission::org_admission_class,
    scope_missing: awaken_protocol_a2a::org_admission::org_admission_scope_missing,
    unavailable: awaken_protocol_a2a::org_admission::org_admission_unavailable,
    rejected: awaken_protocol_a2a::org_admission::org_admission_rejected,
    annotate: None,
};

/// Admit `router`'s metered requests by Org bucket. Mount it inside the edge
/// that stamps the trusted Workspace scope. `None` is a deterministic scenario
/// host that mounts no limiter.
pub(crate) fn admitted(
    router: Router,
    admission: Option<&Arc<dyn OrgRequestAdmission>>,
    encoding: OrgAdmissionEncoding,
) -> Router {
    match admission {
        Some(admission) => router.layer(axum::middleware::from_fn_with_state(
            OrgAdmissionEdge::new(admission.clone(), encoding),
            admit_by_org_bucket,
        )),
        None => router,
    }
}
