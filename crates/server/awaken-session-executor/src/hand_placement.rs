//! `HandPlacement` — where a run's tool calls execute (ADR-0044/0046).
//!
//! Groups the two placement fields that were flat (and DUPLICATED across both
//! [`crate::SharedHost`] and each `SessionCtx`) behind one type owning their
//! precedence invariant: a per-run `provider` that PLACES a run (returns `Some`)
//! overrides the session-wide `remote_hand`; a run the provider declines falls back to
//! the hand; no hand → the kernel's in-process executor. Keeping the pair split meant
//! the precedence lived implicitly in the order two methods happened to apply them, and
//! the pair was cloned field-by-field from the host into every session context.

use std::sync::Arc;

use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::tool::{ToolExecutor, ToolExecutorProvider};

/// The tool-execution placement for a host/session: an optional session-wide hand and
/// an optional per-run placement provider. See the module docs for the precedence.
#[derive(Default, Clone)]
pub(crate) struct HandPlacement {
    /// Session-wide remote hand (ADR-0044): every run routes tool calls here when set.
    /// `None` = the kernel's in-process `LocalToolExecutor`.
    remote_hand: Option<Arc<dyn ToolExecutor>>,
    /// Per-run placement provider (ADR-0046): overrides `remote_hand` for runs it
    /// places. `None` leaves `remote_hand`/in-process behavior unchanged.
    provider: Option<Arc<dyn ToolExecutorProvider>>,
}

impl HandPlacement {
    /// In-process default: no session hand, no placement provider.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Wire a session-wide remote hand (ADR-0044).
    pub(crate) fn set_remote_hand(&mut self, hand: Arc<dyn ToolExecutor>) {
        self.remote_hand = Some(hand);
    }

    /// Wire a per-run placement provider (ADR-0046).
    pub(crate) fn set_provider(&mut self, provider: Arc<dyn ToolExecutorProvider>) {
        self.provider = Some(provider);
    }

    /// The session-wide hand applied to every run, if one is wired. Lower precedence
    /// than [`Self::placed`].
    pub(crate) fn session_hand(&self) -> Option<&Arc<dyn ToolExecutor>> {
        self.remote_hand.as_ref()
    }

    /// The per-run executor override, if a provider is installed and places this run.
    /// Takes precedence over [`Self::session_hand`]; `None` falls back to it.
    pub(crate) async fn placed(&self, activation: &RunActivation) -> Option<Arc<dyn ToolExecutor>> {
        match &self.provider {
            Some(provider) => provider.provide(activation).await,
            None => None,
        }
    }
}
