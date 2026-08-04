//! Wire projection state for the Managed protocol adapter.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use awaken_session_application::SessionApplication;

use crate::types::StreamFrame;

use super::SessionRecord;

/// Disposable Managed wire projections over the canonical Session application.
pub struct ManagedState {
    pub(super) application: SessionApplication,
    /// Disposable record→wire projection; durable truth lives in the application repository.
    pub(super) sessions: Mutex<HashMap<String, SessionRecord>>,
    /// Edge-owned Session-to-Workspace wire projection.
    pub(super) owners: Mutex<HashMap<String, String>>,
    pub(super) session_seq: AtomicU64,
    /// Shared by preview and committed wire-event allocation.
    pub(super) event_seq: Arc<AtomicU64>,
    /// Per-Session live wire stream channels.
    pub(super) live: Mutex<HashMap<String, broadcast::Sender<StreamFrame>>>,
}

impl std::ops::Deref for ManagedState {
    type Target = SessionApplication;

    fn deref(&self) -> &Self::Target {
        &self.application
    }
}
