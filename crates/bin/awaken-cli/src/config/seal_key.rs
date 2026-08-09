use std::fs;
use std::path::PathBuf;

use super::file_support::read_or_create_local_key;

#[derive(Clone)]
pub enum SealKeySource {
    NotOwnedByRole,
    Inline(String),
    File(PathBuf),
    LocalFile(PathBuf),
}

impl std::fmt::Debug for SealKeySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOwnedByRole => formatter.write_str("NotOwnedByRole"),
            Self::Inline(_) => formatter.write_str("Inline([REDACTED])"),
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
            Self::LocalFile(path) => formatter.debug_tuple("LocalFile").field(path).finish(),
        }
    }
}

impl SealKeySource {
    pub fn description(&self) -> String {
        match self {
            Self::NotOwnedByRole => "not owned by this role".to_owned(),
            Self::Inline(_) => "config.toml (redacted)".to_owned(),
            Self::File(path) | Self::LocalFile(path) => path.display().to_string(),
        }
    }

    /// Resolve the existing operator key or create the Local-mode key exactly
    /// once with owner-only permissions. The returned bytes are never logged.
    pub fn load_or_create(&self) -> Result<[u8; 32], String> {
        let value = match self {
            Self::NotOwnedByRole => {
                return Err("this process role does not own the Control seal key".to_owned());
            }
            Self::Inline(value) => value.clone(),
            Self::File(path) => fs::read_to_string(path)
                .map_err(|error| format!("read seal key {}: {error}", path.display()))?,
            Self::LocalFile(path) => read_or_create_local_key(path)?,
        };
        awaken_credential_store::parse_seal_key(value.trim())
            .map_err(|reason| format!("invalid control-plane seal key: {reason}"))
    }
}
