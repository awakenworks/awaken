// @formal entrypoint remote_tool.invoke_at_most_once
// @formal entrypoint remote_tool.result_matches_operation
// @formal evidence remote_tool.indeterminate_never_false_success
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
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::HandResult;
use async_trait::async_trait;
use awaken_sync::InvariantLock;
use tokio_util::sync::CancellationToken;

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
    /// Inspect or cancel only the current entry. Neither operation admits execution.
    async fn observe(&self, operation_id: &str, cancel: bool) -> Result<HandResult, String>;
    /// Join the cancellation signal of the exact live executor, never create one.
    async fn cancellation(&self, operation_id: &str) -> Result<CancellationToken, String>;
}

#[derive(Debug, Clone)]
enum Entry {
    Executing(
        tokio::sync::watch::Sender<Option<HandResult>>,
        CancellationToken,
    ),
    Completed(HandResult),
}

/// Missing process-local evidence remains Unknown, even when a durable marker exists.
fn observe_entry(entry: Option<&Entry>, cancel: bool) -> HandResult {
    match entry {
        Some(Entry::Executing(_, cancellation)) => {
            if cancel {
                cancellation.cancel();
            }
            HandResult::Running {
                cancel_requested: cancellation.is_cancelled(),
            }
        }
        Some(Entry::Completed(result)) => HandResult::Observed {
            result: Box::new(result.clone()),
        },
        None => HandResult::Indeterminate,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedgerEntryPhase {
    Absent,
    Executing,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedgerBeginDisposition {
    Claim,
    Join,
    Replay,
}

fn ledger_entry_phase(entry: Option<&Entry>) -> LedgerEntryPhase {
    match entry {
        None => LedgerEntryPhase::Absent,
        Some(Entry::Executing(_, _)) => LedgerEntryPhase::Executing,
        Some(Entry::Completed(_)) => LedgerEntryPhase::Completed,
    }
}

#[must_use]
const fn ledger_completion_admitted(phase: LedgerEntryPhase) -> bool {
    matches!(phase, LedgerEntryPhase::Executing)
}

#[must_use]
const fn ledger_begin_disposition(phase: LedgerEntryPhase) -> LedgerBeginDisposition {
    match phase {
        LedgerEntryPhase::Absent => LedgerBeginDisposition::Claim,
        LedgerEntryPhase::Executing => LedgerBeginDisposition::Join,
        LedgerEntryPhase::Completed => LedgerBeginDisposition::Replay,
    }
}

/// Process-local ledger used by the zero-configuration in-process hand.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryOperationLedger {
    entries: awaken_sync::LeafLock<HashMap<String, Entry>>,
}

#[async_trait]
#[cfg(any(test, feature = "test-support"))]
impl HandOperationLedger for InMemoryOperationLedger {
    async fn begin(&self, operation_id: &str) -> Result<LedgerAdmission, String> {
        let mut entries = self.entries.lock();
        Ok(
            match (
                ledger_begin_disposition(ledger_entry_phase(entries.get(operation_id))),
                entries.get(operation_id),
            ) {
                (LedgerBeginDisposition::Replay, Some(Entry::Completed(result))) => {
                    LedgerAdmission::Cached(result.clone())
                }
                (LedgerBeginDisposition::Join, Some(Entry::Executing(_, _))) => {
                    LedgerAdmission::InFlight
                }
                (LedgerBeginDisposition::Claim, None) => {
                    entries.insert(
                        operation_id.to_string(),
                        Entry::Executing(
                            tokio::sync::watch::channel(None).0,
                            CancellationToken::new(),
                        ),
                    );
                    LedgerAdmission::Execute
                }
                _ => unreachable!("ledger phase disposition preserves the entry shape"),
            },
        )
    }

    async fn wait(&self, operation_id: &str) -> Result<Option<HandResult>, String> {
        let mut result = {
            let entries = self.entries.lock();
            match entries.get(operation_id) {
                Some(Entry::Completed(result)) => return Ok(Some(result.clone())),
                Some(Entry::Executing(result, _)) => result.subscribe(),
                None => return Ok(None),
            }
        };
        result.changed().await.map_err(|_| {
            "live hand operation disappeared before publishing a result".to_string()
        })?;
        Ok(result.borrow().clone())
    }

    async fn observe(&self, operation_id: &str, cancel: bool) -> Result<HandResult, String> {
        let entries = self.entries.lock();
        Ok(observe_entry(entries.get(operation_id), cancel))
    }

    async fn cancellation(&self, operation_id: &str) -> Result<CancellationToken, String> {
        let entries = self.entries.lock();
        match entries.get(operation_id) {
            Some(Entry::Executing(_, cancellation)) => Ok(cancellation.clone()),
            _ => Err("hand operation has no live cancellation binding".into()),
        }
    }

    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        let result_watch = {
            let mut entries = self.entries.lock();
            if !ledger_completion_admitted(ledger_entry_phase(entries.get(operation_id))) {
                return Err("hand operation completion has no matching live claim".to_string());
            }
            let result_watch = match entries.get(operation_id) {
                Some(Entry::Executing(result_watch, _)) => result_watch.clone(),
                Some(Entry::Completed(_)) | None => {
                    unreachable!("completion admission requires the exact executing entry")
                }
            };
            entries.insert(operation_id.to_string(), Entry::Completed(result.clone()));
            result_watch
        };
        let _ = result_watch.send(Some(result.clone()));
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
    local_entries: InvariantLock<HashMap<String, Entry>>,
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
            local_entries: InvariantLock::new(HashMap::new()),
        })
    }

    fn stem(operation_id: &str) -> String {
        // A Linux filename component is commonly limited to 255 bytes. Encoding
        // at most 96 input bytes leaves ample room for the suffix while retaining
        // the legacy lossless filename. Nested Workflow coordinates can be much
        // longer, so those use a bounded SHA-256 stem and persist the complete
        // identity inside the claim file for collision detection.
        if operation_id.len() > LOSSLESS_STEM_ID_LIMIT {
            let digest = awaken_agent_contract::content_fingerprint(operation_id)
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

    async fn sync_root(&self) -> std::io::Result<()> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || awaken_fs_durability::sync_directory(&root))
            .await
            .map_err(std::io::Error::other)?
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
            match (
                ledger_begin_disposition(ledger_entry_phase(entries.get(operation_id))),
                entries.get(operation_id),
            ) {
                (LedgerBeginDisposition::Replay, Some(Entry::Completed(result))) => {
                    return Ok(LedgerAdmission::Cached(result.clone()));
                }
                (LedgerBeginDisposition::Join, Some(Entry::Executing(_, _))) => {
                    return Ok(LedgerAdmission::InFlight);
                }
                (LedgerBeginDisposition::Claim, None) => {}
                _ => unreachable!("ledger phase disposition preserves the entry shape"),
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
            match (
                ledger_begin_disposition(ledger_entry_phase(entries.get(operation_id))),
                entries.get(operation_id),
            ) {
                (LedgerBeginDisposition::Replay, Some(Entry::Completed(result))) => {
                    return Ok(LedgerAdmission::Cached(result.clone()));
                }
                (LedgerBeginDisposition::Join, Some(Entry::Executing(_, _))) => {
                    return Ok(LedgerAdmission::InFlight);
                }
                (LedgerBeginDisposition::Claim, None) => {}
                _ => unreachable!("ledger phase disposition preserves the entry shape"),
            }
            entries.insert(
                operation_id.to_string(),
                Entry::Executing(
                    tokio::sync::watch::channel(None).0,
                    CancellationToken::new(),
                ),
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
            file.sync_all().await?;
            self.sync_root().await
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
                Some(Entry::Executing(result, _)) => result.subscribe(),
                None => return Ok(None),
            }
        };
        result_watch.changed().await.map_err(|_| {
            "live hand operation disappeared before publishing a result".to_string()
        })?;
        Ok(result_watch.borrow().clone())
    }

    async fn observe(&self, operation_id: &str, cancel: bool) -> Result<HandResult, String> {
        let entries = self
            .local_entries
            .lock()
            .map_err(|_| "hand local ledger poisoned".to_string())?;
        Ok(observe_entry(entries.get(operation_id), cancel))
    }

    async fn cancellation(&self, operation_id: &str) -> Result<CancellationToken, String> {
        let entries = self
            .local_entries
            .lock()
            .map_err(|_| "hand local ledger poisoned".to_string())?;
        match entries.get(operation_id) {
            Some(Entry::Executing(_, cancellation)) => Ok(cancellation.clone()),
            _ => Err("hand operation has no live cancellation binding".into()),
        }
    }

    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        let _admission = self.admission.lock().await;
        let phase = {
            let entries = self
                .local_entries
                .lock()
                .map_err(|_| "hand local ledger poisoned".to_string())?;
            ledger_entry_phase(entries.get(operation_id))
        };
        if !ledger_completion_admitted(phase) {
            return Err("hand operation completion has no matching live claim".to_string());
        }
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
        let claim_path = self.claim_path(operation_id);
        let identity = Self::read_claim_identity(&claim_path)
            .await?
            .ok_or_else(|| "hand operation claim is absent during completion".to_string())?;
        if operation_id.len() > LOSSLESS_STEM_ID_LIMIT {
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
        self.sync_root().await.map_err(|error| error.to_string())?;
        Ok(())
    }

    fn finish_local(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        let result_watch = {
            let mut entries = self
                .local_entries
                .lock()
                .map_err(|_| "hand local ledger poisoned".to_string())?;
            let result_watch = match entries.get(operation_id) {
                Some(Entry::Executing(result_watch, _)) => result_watch.clone(),
                Some(Entry::Completed(_)) | None => {
                    return Err(
                        "hand operation completion lost its matching live claim".to_string()
                    );
                }
            };
            entries.insert(operation_id.to_string(), Entry::Completed(result.clone()));
            result_watch
        };
        let _ = result_watch.send(Some(result.clone()));
        Ok(())
    }

    fn abandon_local(&self, operation_id: &str) -> Result<(), String> {
        let mut entries = self
            .local_entries
            .lock()
            .map_err(|_| "hand local ledger poisoned".to_string())?;
        if matches!(entries.get(operation_id), Some(Entry::Executing(_, _))) {
            entries.remove(operation_id);
        }
        Ok(())
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_tool.result_matches_operation
    #[kani::proof]
    fn ledger_completion_requires_the_exact_executing_operation_entry() {
        // Cause C1: the same operation id has the live Executing entry.
        // Effect E1: admit completion for that operation; E2: reject completion
        // for an absent or already-completed operation. This is one binary
        // admission boundary, so its two decision rows are exhaustive.
        //
        // | Rule | C1 exact Executing entry | Effect |
        // |---|---|---|
        // | R1 | yes | E1 |
        // | R2 | no  | E2 |
        let tag: u8 = kani::any();
        kani::assume(tag < 3);
        let phase = match tag {
            0 => LedgerEntryPhase::Absent,
            1 => LedgerEntryPhase::Executing,
            _ => LedgerEntryPhase::Completed,
        };
        assert_eq!(ledger_completion_admitted(phase), tag == 1);
    }

    /// @obligation remote_tool.invoke_at_most_once
    #[kani::proof]
    fn ledger_begin_allows_claim_only_from_absent_and_never_from_replay() {
        // Cause C1: operation entry absent; C2: executing; C3: completed.
        // Effects: E1 attempt the single durable claim; E2 join the live owner;
        // E3 replay the cached result. The entry phase is closed and exclusive.
        //
        // | Rule | Phase | Effect |
        // |---|---|---|
        // | R1 | Absent | E1 |
        // | R2 | Executing | E2 |
        // | R3 | Completed | E3 |
        let tag: u8 = kani::any();
        kani::assume(tag < 3);
        let phase = match tag {
            0 => LedgerEntryPhase::Absent,
            1 => LedgerEntryPhase::Executing,
            _ => LedgerEntryPhase::Completed,
        };
        let disposition = ledger_begin_disposition(phase);
        assert_eq!(disposition == LedgerBeginDisposition::Claim, tag == 0);
        assert_eq!(disposition == LedgerBeginDisposition::Join, tag == 1);
        assert_eq!(disposition == LedgerBeginDisposition::Replay, tag == 2);
    }
}
