//! Secret-free observation of a corrupt row isolated by its existing store.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecoveryQuarantine {
    pub session_id: crate::SessionId,
    pub reason: String,
}
