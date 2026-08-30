use super::*;


impl SessionDisposition {
    #[must_use]
    pub const fn denies_activity(&self) -> bool {
        !matches!(self, Self::Active)
    }

    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        matches!(self, Self::Deleting | Self::Deleted)
    }

    #[must_use]
    pub fn archived_at(&self) -> Option<&str> {
        match self {
            Self::Archived { archived_at } => Some(archived_at),
            Self::Active | Self::Deleting | Self::Deleted => None,
        }
    }
}

impl From<&SessionDisposition> for SessionDeleteDispositionClass {
    fn from(value: &SessionDisposition) -> Self {
        match value {
            SessionDisposition::Active => Self::Active,
            SessionDisposition::Archived { .. } => Self::Archived,
            SessionDisposition::Deleting => Self::Deleting,
            SessionDisposition::Deleted => Self::Deleted,
        }
    }
}
