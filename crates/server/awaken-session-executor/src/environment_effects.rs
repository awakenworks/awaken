// @formal evidence session_environment.terminal_fence_admits_only_inflight_binding_receipts
//! Authorized Environment effects and durable publication.
//!
//! The realization executor uses the installed Session's original claim and realization
//! authority, the selected binding sink and the same dispatch execution role.
//! No queue discovery or Host callback participates in publication.

use std::sync::Arc;

use crate::environment_owner::BoundSessionEnvironmentIdentity;
use awaken_session_contract::{
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind,
};

#[cfg(test)]
mod tests;
