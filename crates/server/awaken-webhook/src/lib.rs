//! Webhooks (ADR-0048 / S10): a workspace-scoped projection sink over committed
//! session/agent lifecycle facts. An event carries the resource key plus the
//! tenancy (`organization_id` + `workspace_id`) projected from the session's
//! persisted owner (S3); it is signed (Standard Webhooks) and delivered to every
//! matching subscription, retried, and auto-disabled after repeated failures.
//!
//! The crate is protocol-neutral: it takes the event type / object id / tenancy as
//! data, so it does not depend on the Managed wire crate and can project any
//! committed fact. It is also **storage-neutral**: subscriptions live in the
//! config plane (ADR-0048), reached through the [`SubscriptionSource`] port with
//! secrets already resolved — this crate holds only signing, the event shape, and
//! delivery, no persistence.

mod dispatch;
mod event;
mod signing;

pub use dispatch::{
    DispatchReport, ReqwestSender, ResolvedSubscription, SubscriptionSource, WebhookDispatcher,
    WebhookSender,
};
pub use event::{WebhookEvent, WebhookEventData};
pub use signing::{
    SECRET_PREFIX, SignError, generate_secret, sign_bytes, signature_header, verify,
};
