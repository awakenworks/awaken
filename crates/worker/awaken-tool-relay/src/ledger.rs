//! Idempotency authority for remote hand operations.
//!
//! A transport request id is deliberately not an effect identity. The ledger is
//! keyed by the stable operation id carried by [`crate::HandRequest`], allowing a
//! reconnect or response-loss re-drive to return the recorded result without
//! invoking the tool again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::HandResult;
use async_trait::async_trait;

/// Result of atomically admitting one operation id.
#[derive(Debug, Clone, PartialEq)]
pub enum LedgerAdmission {
    /// This caller owns the first execution and must later call `complete`.
    Execute,
    /// A prior execution completed; return this result without invoking again.
    Cached(HandResult),
    /// A prior owner claimed the operation but did not record a result. Running
    /// the effect again would be unsafe, so the only sound answer is unknown.
    Indeterminate,
}

/// Atomic begin/complete ledger. Implementations must make `begin` single-winner
/// for one operation id.
#[async_trait]
pub trait HandOperationLedger: Send + Sync {
    async fn begin(&self, operation_id: &str) -> Result<LedgerAdmission, String>;
    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String>;
}

#[derive(Debug, Clone)]
enum Entry {
    Executing,
    Completed(HandResult),
}

/// Process-local ledger used by the zero-configuration in-process hand.
#[derive(Default)]
pub struct InMemoryOperationLedger {
    entries: Mutex<HashMap<String, Entry>>,
}

#[async_trait]
impl HandOperationLedger for InMemoryOperationLedger {
    async fn begin(&self, operation_id: &str) -> Result<LedgerAdmission, String> {
        let mut entries = self.entries.lock().expect("hand ledger poisoned");
        Ok(match entries.get(operation_id) {
            Some(Entry::Completed(result)) => LedgerAdmission::Cached(result.clone()),
            Some(Entry::Executing) => LedgerAdmission::Indeterminate,
            None => {
                entries.insert(operation_id.to_string(), Entry::Executing);
                LedgerAdmission::Execute
            }
        })
    }

    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        self.entries
            .lock()
            .expect("hand ledger poisoned")
            .insert(operation_id.to_string(), Entry::Completed(result.clone()));
        Ok(())
    }
}

/// Filesystem-backed ledger for a hand whose effects must remain fenced across
/// process restarts. `create_new` on the claim file is the single-winner CAS; a
/// claim without a result is intentionally indeterminate after a crash.
pub struct FsOperationLedger {
    root: PathBuf,
}

impl FsOperationLedger {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        Ok(Self { root })
    }

    fn stem(operation_id: &str) -> Result<String, String> {
        // A Linux filename component is commonly limited to 255 bytes. Encoding
        // at most 96 input bytes leaves ample room for the suffix while retaining
        // the complete operation identity: unlike a hash, distinct accepted ids
        // cannot alias the same ledger entry.
        if operation_id.len() > 96 {
            return Err("hand operation id exceeds the durable-ledger limit".into());
        }

        let mut stem = String::with_capacity(operation_id.len() * 2);
        for byte in operation_id.as_bytes() {
            use std::fmt::Write as _;
            write!(&mut stem, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(stem)
    }

    fn claim_path(&self, operation_id: &str) -> Result<PathBuf, String> {
        Ok(self
            .root
            .join(format!("{}.claim", Self::stem(operation_id)?)))
    }

    fn result_path(&self, operation_id: &str) -> Result<PathBuf, String> {
        Ok(self
            .root
            .join(format!("{}.result", Self::stem(operation_id)?)))
    }

    async fn read_result(path: &Path) -> Result<Option<HandResult>, String> {
        match tokio::fs::read(path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("decode hand ledger result: {error}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[async_trait]
impl HandOperationLedger for FsOperationLedger {
    async fn begin(&self, operation_id: &str) -> Result<LedgerAdmission, String> {
        let result_path = self.result_path(operation_id)?;
        if let Some(result) = Self::read_result(&result_path).await? {
            return Ok(LedgerAdmission::Cached(result));
        }

        let claim_path = self.claim_path(operation_id)?;
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&claim_path)
            .await
        {
            Ok(_) => Ok(LedgerAdmission::Execute),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Completion may have raced the first result read.
                Ok(match Self::read_result(&result_path).await? {
                    Some(result) => LedgerAdmission::Cached(result),
                    None => LedgerAdmission::Indeterminate,
                })
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        let path = self.result_path(operation_id)?;
        let temporary = path.with_extension("result.tmp");
        let bytes = serde_json::to_vec(result).map_err(|error| error.to_string())?;
        tokio::fs::write(&temporary, bytes)
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(|error| error.to_string())
    }
}
