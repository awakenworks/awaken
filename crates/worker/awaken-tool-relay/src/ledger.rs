//! Idempotency authority for remote hand operations.
//!
//! A transport request id is deliberately not an effect identity. The ledger is
//! keyed by the stable operation id carried by [`crate::HandRequest`], allowing a
//! reconnect or response-loss re-drive to return the process-local result
//! without invoking the tool again. Durable storage contains only fencing
//! metadata: tool outputs can contain credentials and must never be exposed to
//! another process in the sandbox container.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::HandResult;
use async_trait::async_trait;

const LOSSLESS_STEM_ID_LIMIT: usize = 96;
pub const DEFAULT_FS_LEDGER_MAX_ENTRIES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimIdentity {
    Match,
    Interrupted,
}

/// Result of atomically admitting one operation id.
#[derive(Debug, Clone, PartialEq)]
pub enum LedgerAdmission {
    /// This caller owns the first execution and must later call `complete`.
    Execute,
    /// A prior execution completed; return this result without invoking again.
    Cached(HandResult),
    /// The same live Hand process is already executing this operation. A
    /// reconnect must join that execution rather than report a crash or replay.
    InFlight,
    /// A prior owner claimed the operation but did not record a result. Running
    /// the effect again would be unsafe, so the only sound answer is unknown.
    Indeterminate,
}

/// Atomic begin/complete ledger. Implementations must make `begin` single-winner
/// for one operation id.
#[async_trait]
pub trait HandOperationLedger: Send + Sync {
    async fn begin(&self, operation_id: &str) -> Result<LedgerAdmission, String>;
    /// Wait for an operation admitted as [`LedgerAdmission::InFlight`]. `None`
    /// means the live owner disappeared without a result and replay is unsafe.
    async fn wait(&self, operation_id: &str) -> Result<Option<HandResult>, String>;
    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String>;
}

#[derive(Debug, Clone)]
enum Entry {
    Executing(tokio::sync::watch::Sender<Option<HandResult>>),
    Completed(HandResult),
}

/// Process-local ledger used by the zero-configuration in-process hand.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryOperationLedger {
    entries: Mutex<HashMap<String, Entry>>,
}

#[async_trait]
#[cfg(any(test, feature = "test-support"))]
impl HandOperationLedger for InMemoryOperationLedger {
    async fn begin(&self, operation_id: &str) -> Result<LedgerAdmission, String> {
        let mut entries = self.entries.lock().expect("hand ledger poisoned");
        Ok(match entries.get(operation_id) {
            Some(Entry::Completed(result)) => LedgerAdmission::Cached(result.clone()),
            Some(Entry::Executing(_)) => LedgerAdmission::InFlight,
            None => {
                entries.insert(
                    operation_id.to_string(),
                    Entry::Executing(tokio::sync::watch::channel(None).0),
                );
                LedgerAdmission::Execute
            }
        })
    }

    async fn wait(&self, operation_id: &str) -> Result<Option<HandResult>, String> {
        let mut result = {
            let entries = self.entries.lock().expect("hand ledger poisoned");
            match entries.get(operation_id) {
                Some(Entry::Completed(result)) => return Ok(Some(result.clone())),
                Some(Entry::Executing(result)) => result.subscribe(),
                None => return Ok(None),
            }
        };
        result.changed().await.map_err(|_| {
            "live hand operation disappeared before publishing a result".to_string()
        })?;
        Ok(result.borrow().clone())
    }

    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        let result_watch = {
            let mut entries = self.entries.lock().expect("hand ledger poisoned");
            let result_watch = match entries.get(operation_id) {
                Some(Entry::Executing(result_watch)) => Some(result_watch.clone()),
                Some(Entry::Completed(_)) | None => None,
            };
            entries.insert(operation_id.to_string(), Entry::Completed(result.clone()));
            result_watch
        };
        if let Some(result_watch) = result_watch {
            let _ = result_watch.send(Some(result.clone()));
        }
        Ok(())
    }
}

/// Filesystem-backed ledger for a hand whose effects must remain fenced across
/// process restarts. `create_new` on the claim file is the single-winner CAS; a
/// claim from any prior process is intentionally indeterminate after a crash.
/// Completion payloads remain process-local; the filesystem stores only an
/// opaque completion marker so sandbox siblings cannot read tool output.
pub struct FsOperationLedger {
    root: PathBuf,
    max_entries: usize,
    durable_entries: AtomicUsize,
    /// Serializes only durable admission, never tool execution. This makes the
    /// capacity check and claim-file CAS one local transaction without holding
    /// a synchronous mutex across filesystem awaits.
    admission: tokio::sync::Mutex<()>,
    /// Process-local join points and completed results. Durable storage never
    /// contains a `HandResult`: an ACP or tool process in the same container can
    /// share the Hand's uid and must not be able to read credentials or output.
    local_entries: Mutex<HashMap<String, Entry>>,
}

impl FsOperationLedger {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, String> {
        Self::open_with_max_entries(root, DEFAULT_FS_LEDGER_MAX_ENTRIES)
    }

    pub fn open_with_max_entries(
        root: impl Into<PathBuf>,
        max_entries: usize,
    ) -> Result<Self, String> {
        if max_entries == 0 {
            return Err("hand operation ledger capacity must be positive".into());
        }
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let durable_entries = std::fs::read_dir(&root)
            .map_err(|error| error.to_string())?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "claim")
            })
            .count();
        Ok(Self {
            root,
            max_entries,
            durable_entries: AtomicUsize::new(durable_entries),
            admission: tokio::sync::Mutex::new(()),
            local_entries: Mutex::new(HashMap::new()),
        })
    }

    fn stem(operation_id: &str) -> String {
        // A Linux filename component is commonly limited to 255 bytes. Encoding
        // at most 96 input bytes leaves ample room for the suffix while retaining
        // the legacy lossless filename. Nested Workflow coordinates can be much
        // longer, so those use a bounded SHA-256 stem and persist the complete
        // identity inside the claim file for collision detection.
        if operation_id.len() > LOSSLESS_STEM_ID_LIMIT {
            let digest = awaken_runtime_contract::resolution::content_fingerprint(operation_id)
                .expect("a string operation identity always serializes");
            return format!("sha256-{digest}");
        }

        let mut stem = String::with_capacity(operation_id.len() * 2);
        for byte in operation_id.as_bytes() {
            use std::fmt::Write as _;
            write!(&mut stem, "{byte:02x}").expect("writing to a String cannot fail");
        }
        stem
    }

    fn claim_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join(format!("{}.claim", Self::stem(operation_id)))
    }

    fn result_path(&self, operation_id: &str) -> PathBuf {
        self.root
            .join(format!("{}.result", Self::stem(operation_id)))
    }

    async fn read_claim_identity(path: &Path) -> Result<Option<Vec<u8>>, String> {
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn validate_hashed_claim(operation_id: &str, identity: &[u8]) -> Result<ClaimIdentity, String> {
        if identity == operation_id.as_bytes() {
            Ok(ClaimIdentity::Match)
        } else if identity.is_empty() {
            Ok(ClaimIdentity::Interrupted)
        } else {
            Err("hand operation digest collision in durable ledger".into())
        }
    }
}

#[async_trait]
impl HandOperationLedger for FsOperationLedger {
    async fn begin(&self, operation_id: &str) -> Result<LedgerAdmission, String> {
        let _admission = self.admission.lock().await;
        let hashed = operation_id.len() > LOSSLESS_STEM_ID_LIMIT;
        let claim_path = self.claim_path(operation_id);
        {
            let entries = self
                .local_entries
                .lock()
                .map_err(|_| "hand local ledger poisoned".to_string())?;
            match entries.get(operation_id) {
                Some(Entry::Completed(result)) => {
                    return Ok(LedgerAdmission::Cached(result.clone()));
                }
                Some(Entry::Executing(_)) => return Ok(LedgerAdmission::InFlight),
                None => {}
            }
        }
        if let Some(identity) = Self::read_claim_identity(&claim_path).await? {
            if hashed
                && Self::validate_hashed_claim(operation_id, &identity)?
                    == ClaimIdentity::Interrupted
            {
                return Ok(LedgerAdmission::Indeterminate);
            }
            return Ok(LedgerAdmission::Indeterminate);
        }
        if self.durable_entries.load(Ordering::Acquire) >= self.max_entries {
            return Err(format!(
                "hand operation ledger capacity {} is exhausted",
                self.max_entries
            ));
        }

        {
            let mut entries = self
                .local_entries
                .lock()
                .map_err(|_| "hand local ledger poisoned".to_string())?;
            match entries.get(operation_id) {
                Some(Entry::Completed(result)) => {
                    return Ok(LedgerAdmission::Cached(result.clone()));
                }
                Some(Entry::Executing(_)) => return Ok(LedgerAdmission::InFlight),
                None => {}
            }
            entries.insert(
                operation_id.to_string(),
                Entry::Executing(tokio::sync::watch::channel(None).0),
            );
        }

        let persisted = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&claim_path)
                .await?;
            if hashed {
                use tokio::io::AsyncWriteExt as _;
                file.write_all(operation_id.as_bytes()).await?;
            }
            file.sync_all().await
        }
        .await;
        match persisted {
            Ok(()) => {
                self.durable_entries.fetch_add(1, Ordering::AcqRel);
                Ok(LedgerAdmission::Execute)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.abandon_local(operation_id)?;
                if hashed {
                    let identity =
                        Self::read_claim_identity(&claim_path)
                            .await?
                            .ok_or_else(|| {
                                "hand operation claim disappeared during admission".to_string()
                            })?;
                    if Self::validate_hashed_claim(operation_id, &identity)?
                        == ClaimIdentity::Interrupted
                    {
                        return Ok(LedgerAdmission::Indeterminate);
                    }
                }
                Ok(LedgerAdmission::Indeterminate)
            }
            Err(error) => {
                self.abandon_local(operation_id)?;
                Err(error.to_string())
            }
        }
    }

    async fn wait(&self, operation_id: &str) -> Result<Option<HandResult>, String> {
        let mut result_watch = {
            let entries = self
                .local_entries
                .lock()
                .map_err(|_| "hand local ledger poisoned".to_string())?;
            match entries.get(operation_id) {
                Some(Entry::Completed(result)) => return Ok(Some(result.clone())),
                Some(Entry::Executing(result)) => result.subscribe(),
                None => return Ok(None),
            }
        };
        result_watch.changed().await.map_err(|_| {
            "live hand operation disappeared before publishing a result".to_string()
        })?;
        Ok(result_watch.borrow().clone())
    }

    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        let completion = self.write_result(operation_id, result).await;
        match completion {
            Ok(()) => self.finish_local(operation_id, result),
            Err(error) => {
                self.abandon_local(operation_id)?;
                Err(error)
            }
        }
    }
}

impl FsOperationLedger {
    async fn write_result(&self, operation_id: &str, _result: &HandResult) -> Result<(), String> {
        if operation_id.len() > LOSSLESS_STEM_ID_LIMIT {
            let claim_path = self.claim_path(operation_id);
            let identity = Self::read_claim_identity(&claim_path)
                .await?
                .ok_or_else(|| "hand operation claim is absent during completion".to_string())?;
            match Self::validate_hashed_claim(operation_id, &identity)? {
                ClaimIdentity::Match => {}
                ClaimIdentity::Interrupted => {
                    return Err(
                        "hand operation claim is indeterminate after an interrupted durable write"
                            .into(),
                    );
                }
            }
        }
        let path = self.result_path(operation_id);
        let temporary = path.with_extension("result.tmp");
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .await
            .map_err(|error| error.to_string())?;
        use tokio::io::AsyncWriteExt as _;
        file.write_all(b"completed-v1\n")
            .await
            .map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(|error| error.to_string())?;
        let directory = tokio::fs::File::open(&self.root)
            .await
            .map_err(|error| error.to_string())?;
        directory
            .sync_all()
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn finish_local(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        let result_watch = {
            let mut entries = self
                .local_entries
                .lock()
                .map_err(|_| "hand local ledger poisoned".to_string())?;
            let result_watch = match entries.get(operation_id) {
                Some(Entry::Executing(result_watch)) => Some(result_watch.clone()),
                Some(Entry::Completed(_)) | None => None,
            };
            entries.insert(operation_id.to_string(), Entry::Completed(result.clone()));
            result_watch
        };
        if let Some(result_watch) = result_watch {
            let _ = result_watch.send(Some(result.clone()));
        }
        Ok(())
    }

    fn abandon_local(&self, operation_id: &str) -> Result<(), String> {
        let mut entries = self
            .local_entries
            .lock()
            .map_err(|_| "hand local ledger poisoned".to_string())?;
        if matches!(entries.get(operation_id), Some(Entry::Executing(_))) {
            entries.remove(operation_id);
        }
        Ok(())
    }
}
