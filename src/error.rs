use thiserror::Error;

use crate::model::Phase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownKeyPolicy {
    Error,
    Skip,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StateError {
    #[error("revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("patch base revision mismatch: {left} vs {right}")]
    MutationBaseRevisionMismatch { left: u64, right: u64 },
    #[error("plugin already installed: {name}")]
    PluginAlreadyInstalled { name: String },
    #[error("plugin not installed: {type_name}")]
    PluginNotInstalled { type_name: &'static str },
    #[error("key already registered: {key}")]
    KeyAlreadyRegistered { key: String },
    #[error("unknown key: {key}")]
    UnknownKey { key: String },
    #[error("failed to decode key {key}: {message}")]
    KeyDecode { key: String, message: String },
    #[error("failed to encode key {key}: {message}")]
    KeyEncode { key: String, message: String },
    #[error("scheduled action handler already registered: {key}")]
    HandlerAlreadyRegistered { key: String },
    #[error("effect handler already registered: {key}")]
    EffectHandlerAlreadyRegistered { key: String },
    #[error("phase {phase:?} did not converge after {max_rounds} rounds")]
    PhaseRunLoopExceeded { phase: Phase, max_rounds: usize },
    #[error("no scheduled action handler registered for {key}")]
    UnknownScheduledActionHandler { key: String },
    #[error("no effect handler registered for {key}")]
    UnknownEffectHandler { key: String },
    #[error("parallel merge conflict on exclusive key: {key}")]
    ParallelMergeConflict { key: String },
}
