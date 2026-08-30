use super::*;


impl SessionRepositoryError {
    #[must_use]
    pub const fn recovery_action(&self) -> SessionRepositoryRecoveryAction {
        match self {
            Self::Unavailable(_) => SessionRepositoryRecoveryAction::Retry,
            Self::Corrupt(_) => SessionRepositoryRecoveryAction::Quarantine,
            Self::NotFound | Self::Conflict(_) | Self::InvalidMutation(_) => {
                SessionRepositoryRecoveryAction::Reject
            }
        }
    }
}
