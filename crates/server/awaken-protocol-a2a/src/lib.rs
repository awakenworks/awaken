//! `awaken-protocol-a2a` — the A2A (Agent2Agent) v1.0 protocol adapter.
//!
//! The anti-corruption boundary between the public A2A wire (`message:send` over
//! HTTP+JSON, returning a `Task`) and the neutral runtime. It owns the A2A DTOs,
//! the projection from committed `Message`s to an A2A `Task`, and the axum router;
//! it drives the shared neutral `ProtocolRuntime` port (from
//! `awaken-protocol-transport`) and constructs no runtime itself. It is the only
//! crate permitted to name A2A protocol vocabulary.
//!
//! It implements the A2A 0.3 and 1.0 JSON-RPC and HTTP+JSON bindings, including
//! request/response sends, SSE streaming and task resubscription, task lifecycle,
//! tenant fencing, and push-notification configuration and webhook delivery. It
//! shares the same neutral host as every other adapter, so callers through A2A
//! and the managed protocols interact with the same runtime state.

pub mod client;
pub mod encoder;
pub mod request;
pub mod router;
mod state;
mod time;
pub mod types;
mod v1;

pub use awaken_credential::{AuthChallenge, Credential, CredentialRefresher};
pub use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
pub use client::{ClientError, HttpTransport, Response, Transport};
pub use router::{agent_card, router};
pub use types::{
    AgentCard, AgentInterface, ApiKeyLocation, Artifact, AuthenticationInfo, AuthorizationCodeFlow,
    ClientCredentialsFlow, ImplicitFlow, ListPushNotificationConfigsResponse, OAuthFlows,
    PasswordFlow, PushNotificationConfig, SecurityScheme, SendMessageConfiguration,
    SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskPushNotificationConfig, TaskState, TaskStatusUpdateEvent,
};
