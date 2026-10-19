//! Container-tier vocabulary shared by container backends, the Gateway and the
//! Session owner: the allowlist capability wire, Kubernetes capacity and
//! continuation-volume policy, the live-input projection root, and the
//! Kubernetes Create broker wire and privileged effects role.
//!
//! The provider's vocabulary -- realization intent, adoption, observation
//! purpose, effect ownership, the live [`awaken_provisioning_contract::SessionSandbox`]
//! and its participants -- is the provisioning contract's; this crate names it
//! from there. Provider-owned durable handle payloads stay opaque at this seam.

//! Shared Create observations have this one public owner. Encoding an Unknown
//! join transports an observation and grants no physical completion authority:
//! ```
//! use awaken_container_contract::{CreateReplyProfile, RequestJoin, TicketResult};
//! let result = TicketResult::Created { uid: "original-uid".into() };
//! result.validate().unwrap();
//! let _ = CreateReplyProfile::WholeJoin.encode_body(&RequestJoin::Unknown).unwrap();
//! ```

mod create;
mod create_broker;
mod create_metadata;
mod create_reply;
mod create_ticket;
mod host;
mod k8s_capacity;
mod policy;
mod runtime_error;

pub use create::{K8sCreateRequest, K8sCreateResponse};
pub use create_broker::{K8sCreateBrokerEffects, RawCreateFailurePhase};
pub use host::{AllowlistCapability, normalize_allowlist_host};
pub use k8s_capacity::{
    DEFAULT_K8S_UNSCHEDULABLE_WAIT, K8sPodReadiness, K8sPodScheduledCondition, K8sPodWait,
    k8s_pod_age, k8s_pod_provisioning_deadline_ms, k8s_pod_unschedulable, k8s_pod_wait,
    kubernetes_refusal_is_capacity,
};
pub use policy::{
    K8sContinuationVolume, K8sImagePrewarm, LIVE_INPUTS_ROOT, REPOSITORY_GATEWAY_CA_PATH,
    live_input_relative_path,
};
pub use runtime_error::RuntimeError;

pub use create_metadata::{
    ISOLATION_ID_ANNOTATION, ISOLATION_LABEL, ORIGINAL_GUARD_FINALIZER, REQUEST_DIGEST_ANNOTATION,
    original_request_metadata_matches,
};
pub use create_reply::{CreateReplyProfile, K8S_CREATE_CAPTURE_HEADER, K8S_CREATE_WIRE_VERSION};
pub use create_ticket::{
    CreateTicket, CreateTicketRequest, K8sLedgerBinding, RequestJoin, TicketAdmission,
    TicketOrigin, TicketResult, TicketStatus,
};
