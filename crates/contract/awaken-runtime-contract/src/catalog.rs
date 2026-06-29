use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCatalogInstall {
    pub publication_id: String,
    pub fingerprint: crate::resolved::CatalogFingerprint,
    pub source_revisions: Vec<String>,
    pub capabilities: crate::capability::RuntimeCapabilityCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledCatalog {
    pub fingerprint: crate::resolved::CatalogFingerprint,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("catalog install rejected: {0}")]
    Rejected(String),
}

pub trait RuntimeCatalogInstaller {
    fn install_catalog(&self, install: RuntimeCatalogInstall) -> Result<InstalledCatalog, Error>;
}
