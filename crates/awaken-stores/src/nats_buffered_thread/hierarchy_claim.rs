//! Distributed hierarchy write claim for NATS buffered thread mutations.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_nats::jetstream::kv::{CreateErrorKind, UpdateErrorKind};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use awaken_server_contract::contract::storage::StorageError;

use super::keys;

const CLAIM_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const CLAIM_BACKOFF_START: std::time::Duration = std::time::Duration::from_micros(200);
const CLAIM_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(20);
const DEFAULT_CLAIM_LEASE_MS: u64 = 30_000;
const DEFAULT_CLAIM_RENEW_INTERVAL_MS: u64 = 10_000;

#[derive(Debug, Clone, Default)]
pub(crate) struct ClaimOptions {
    inner: Arc<ClaimOptionsInner>,
}

#[derive(Debug, Default)]
struct ClaimOptionsInner {
    lease_ms: AtomicU64,
    renew_interval_ms: AtomicU64,
}

impl ClaimOptions {
    pub(crate) fn lease_ms(&self) -> u64 {
        let configured = self.inner.lease_ms.load(Ordering::Relaxed);
        if configured == 0 {
            DEFAULT_CLAIM_LEASE_MS
        } else {
            configured
        }
    }

    pub(crate) fn renew_interval(&self) -> Option<std::time::Duration> {
        let configured = self.inner.renew_interval_ms.load(Ordering::Relaxed);
        let renew_interval_ms = if configured == 0 {
            DEFAULT_CLAIM_RENEW_INTERVAL_MS
        } else {
            configured
        };
        if renew_interval_ms == u64::MAX {
            None
        } else {
            Some(std::time::Duration::from_millis(renew_interval_ms))
        }
    }

    pub(crate) fn set_for_tests(&self, lease_ms: u64, renew_interval_ms: Option<u64>) {
        self.inner.lease_ms.store(lease_ms, Ordering::Relaxed);
        self.inner
            .renew_interval_ms
            .store(renew_interval_ms.unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(crate) struct AcquiredClaim {
    claim_name: &'static str,
    claim_key: String,
    claim_token: String,
    renew_task: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) type AcquiredHierarchyClaim = AcquiredClaim;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HierarchyClaim {
    claim_token: String,
    #[serde(default)]
    lease_until: u64,
}

impl AcquiredClaim {
    pub(crate) fn claim_token(&self) -> &str {
        &self.claim_token
    }

    pub(crate) async fn ensure_current(
        &self,
        kv: &async_nats::jetstream::kv::Store,
    ) -> Result<(), StorageError> {
        let entry = kv
            .entry(&self.claim_key)
            .await
            .map_err(|error| StorageError::Io(format!("{} entry: {error}", self.claim_name)))?;
        let Some(entry) = entry else {
            return Err(StorageError::Io(format!(
                "{} lost ownership: entry missing",
                self.claim_name
            )));
        };
        let claim = decode_claim(&entry.value)?;
        if claim.claim_token != self.claim_token {
            return Err(StorageError::Io(format!(
                "{} lost ownership to another writer",
                self.claim_name
            )));
        }
        if claim.lease_until < now_millis() {
            return Err(StorageError::Io(format!(
                "{} expired before operation completed",
                self.claim_name
            )));
        }
        Ok(())
    }
}

pub(crate) async fn claim_token_is_current(
    kv: &async_nats::jetstream::kv::Store,
    claim_token: &str,
) -> Result<bool, StorageError> {
    claim_token_is_current_for_key(
        kv,
        keys::hierarchy_lock_key(),
        "hierarchy claim",
        claim_token,
    )
    .await
}

pub(crate) async fn claim_token_is_current_for_key(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
    claim_name: &'static str,
    claim_token: &str,
) -> Result<bool, StorageError> {
    let entry = kv
        .entry(key)
        .await
        .map_err(|error| StorageError::Io(format!("{claim_name} entry: {error}")))?;
    let Some(entry) = entry else {
        return Ok(false);
    };
    let claim = decode_claim(&entry.value)?;
    Ok(claim.claim_token == claim_token && claim.lease_until >= now_millis())
}

fn encode_claim(claim: &HierarchyClaim) -> Result<Bytes, StorageError> {
    serde_json::to_vec(claim)
        .map(Bytes::from)
        .map_err(|error| StorageError::Serialization(error.to_string()))
}

fn decode_claim(bytes: &[u8]) -> Result<HierarchyClaim, StorageError> {
    if bytes.is_empty() {
        return Ok(HierarchyClaim::default());
    }
    serde_json::from_slice(bytes).map_err(|error| StorageError::Serialization(error.to_string()))
}

fn is_retryable_create_error(kind: CreateErrorKind) -> bool {
    matches!(kind, CreateErrorKind::AlreadyExists)
}

fn is_retryable_update_error(kind: UpdateErrorKind) -> bool {
    matches!(kind, UpdateErrorKind::WrongLastRevision)
}

pub(crate) async fn acquire(
    kv: &async_nats::jetstream::kv::Store,
    options: &ClaimOptions,
) -> Result<AcquiredHierarchyClaim, StorageError> {
    acquire_for_key(kv, keys::hierarchy_lock_key(), "hierarchy claim", options).await
}

pub(crate) async fn acquire_for_key(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
    claim_name: &'static str,
    options: &ClaimOptions,
) -> Result<AcquiredClaim, StorageError> {
    let deadline = std::time::Instant::now() + CLAIM_TOTAL_TIMEOUT;
    let mut backoff = CLAIM_BACKOFF_START;
    let mut attempts = 0u32;
    let lease_ms = options.lease_ms();

    loop {
        attempts += 1;
        let entry = kv
            .entry(key)
            .await
            .map_err(|error| StorageError::Io(format!("{claim_name} entry: {error}")))?;
        let (existing, revision) = match entry {
            Some(entry) => (decode_claim(&entry.value)?, entry.revision),
            None => (HierarchyClaim::default(), 0),
        };

        let now = now_millis();
        if existing.lease_until >= now {
            if std::time::Instant::now() >= deadline {
                return Err(StorageError::Io(format!(
                    "{claim_name} timeout after {attempts} attempts"
                )));
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(backoff.saturating_mul(2), CLAIM_BACKOFF_MAX);
            continue;
        }

        let claim = HierarchyClaim {
            claim_token: uuid::Uuid::now_v7().to_string(),
            lease_until: now.saturating_add(lease_ms),
        };
        let claim_token = claim.claim_token.clone();
        let bytes = encode_claim(&claim)?;
        let acquired = if revision == 0 {
            match kv.create(key, bytes).await {
                Ok(_) => true,
                Err(error) if is_retryable_create_error(error.kind()) => false,
                Err(error) => {
                    return Err(StorageError::Io(format!(
                        "{claim_name} create claim: {error}"
                    )));
                }
            }
        } else {
            match kv.update(key, bytes, revision).await {
                Ok(_) => true,
                Err(error) if is_retryable_update_error(error.kind()) => false,
                Err(error) => {
                    return Err(StorageError::Io(format!(
                        "{claim_name} update claim: {error}"
                    )));
                }
            }
        };
        if acquired {
            let renew_task = spawn_renew_task(
                kv.clone(),
                key.to_string(),
                claim_name,
                options.clone(),
                claim_token.clone(),
            );
            return Ok(AcquiredClaim {
                claim_name,
                claim_key: key.to_string(),
                claim_token,
                renew_task,
            });
        }

        if std::time::Instant::now() >= deadline {
            return Err(StorageError::Io(format!(
                "{claim_name} timeout after {attempts} attempts"
            )));
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.saturating_mul(2), CLAIM_BACKOFF_MAX);
    }
}

pub(crate) async fn release(
    kv: &async_nats::jetstream::kv::Store,
    mut claim: AcquiredClaim,
) -> Result<(), StorageError> {
    if let Some(renew_task) = claim.renew_task.take() {
        renew_task.abort();
        let _ = renew_task.await;
    }

    for _ in 0..5 {
        let entry = kv
            .entry(&claim.claim_key)
            .await
            .map_err(|error| StorageError::Io(format!("{} entry: {error}", claim.claim_name)))?;
        let Some(entry) = entry else {
            return Ok(());
        };
        let mut existing = decode_claim(&entry.value)?;
        if existing.claim_token != claim.claim_token {
            return Ok(());
        }
        existing.lease_until = 0;
        let bytes = encode_claim(&existing)?;
        match kv.update(&claim.claim_key, bytes, entry.revision).await {
            Ok(_) => return Ok(()),
            Err(error) if is_retryable_update_error(error.kind()) => {}
            Err(error) => {
                return Err(StorageError::Io(format!(
                    "{} release update: {error}",
                    claim.claim_name
                )));
            }
        }
    }

    Err(StorageError::Io(format!(
        "{} release CAS exhausted retries",
        claim.claim_name
    )))
}

fn spawn_renew_task(
    kv: async_nats::jetstream::kv::Store,
    claim_key: String,
    claim_name: &'static str,
    options: ClaimOptions,
    claim_token: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let renew_interval = options.renew_interval()?;
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(renew_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match renew(
                &kv,
                &claim_key,
                claim_name,
                &claim_token,
                options.lease_ms(),
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    tracing::warn!(error = %error, claim_name, "distributed claim renew failed");
                    break;
                }
            }
        }
    }))
}

async fn renew(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
    claim_name: &'static str,
    claim_token: &str,
    lease_ms: u64,
) -> Result<bool, StorageError> {
    for _ in 0..5 {
        let entry = kv
            .entry(key)
            .await
            .map_err(|error| StorageError::Io(format!("{claim_name} entry: {error}")))?;
        let Some(entry) = entry else {
            return Ok(false);
        };
        let mut existing = decode_claim(&entry.value)?;
        if existing.claim_token != claim_token {
            return Ok(false);
        }
        existing.lease_until = now_millis().saturating_add(lease_ms);
        let bytes = encode_claim(&existing)?;
        match kv.update(key, bytes, entry.revision).await {
            Ok(_) => return Ok(true),
            Err(error) if is_retryable_update_error(error.kind()) => {}
            Err(error) => {
                return Err(StorageError::Io(format!(
                    "{claim_name} renew update: {error}"
                )));
            }
        }
    }

    Err(StorageError::Io(format!(
        "{claim_name} renew CAS exhausted retries"
    )))
}

use super::recovery::now_millis;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_create_retryable_only_for_existing_key_conflicts() {
        assert!(is_retryable_create_error(CreateErrorKind::AlreadyExists));
        assert!(!is_retryable_create_error(CreateErrorKind::Publish));
        assert!(!is_retryable_create_error(CreateErrorKind::Ack));
        assert!(!is_retryable_create_error(CreateErrorKind::InvalidKey));
        assert!(!is_retryable_create_error(CreateErrorKind::Other));
    }

    #[test]
    fn claim_update_retryable_only_for_revision_conflicts() {
        assert!(is_retryable_update_error(
            UpdateErrorKind::WrongLastRevision
        ));
        assert!(!is_retryable_update_error(UpdateErrorKind::InvalidKey));
        assert!(!is_retryable_update_error(UpdateErrorKind::TimedOut));
        assert!(!is_retryable_update_error(UpdateErrorKind::Other));
    }
}
