// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: The credential owner alone coordinates token acquisition; IO ports retain exact admission and publication responsibility.
//! One token-response, expiry and single-flight executor for every consumer.
//! Request encoding and secret IO are supplied by admitted last-mile ports.
use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    AcquiredCredentialToken, CredentialTokenAcquisition, CredentialTokenIssuer,
    IssuedCredentialTokens, RefreshTokenPolicy, TokenAcquisitionError, TokenEndpointResponse,
    TokenResponseSpec,
};
use awaken_time::{WallClock, WallClockExt as _};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

struct Entry {
    value: tokio::sync::Mutex<Option<AcquiredCredentialToken>>,
    invalidated_through: AtomicU64,
}
impl Entry {
    fn new() -> Self {
        Self {
            value: tokio::sync::Mutex::new(None),
            invalidated_through: AtomicU64::new(0),
        }
    }
}

/// Disposable cache only. Durable grant ownership and paired-token publication
/// remain in the same credential authority's acquisition port/material WAL.
pub struct TokenAcquisitionCoordinator {
    clock: Arc<dyn WallClock>,
    entries: awaken_sync::LeafLock<BTreeMap<String, Arc<Entry>>>,
    generation: AtomicU64,
}
impl TokenAcquisitionCoordinator {
    #[must_use]
    pub fn new(clock: Arc<dyn WallClock>) -> Self {
        Self {
            clock,
            entries: awaken_sync::LeafLock::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
        }
    }
    fn entry(&self, partition: &str) -> Result<Arc<Entry>, TokenAcquisitionError> {
        if partition.is_empty() || partition.len() > 8192 {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.get(partition) {
            return Ok(entry.clone());
        }
        if entries.len() >= 256 {
            let now = self.clock.now_ms();
            entries.retain(|_, e| {
                Arc::strong_count(e) > 1
                    || e.value.try_lock().map_or(true, |v| {
                        v.as_ref().is_some_and(|v| {
                            v.expires_at_unix_ms.is_some_and(|at| at > now)
                                && v.cache_generation.is_some_and(|g| {
                                    g > e.invalidated_through.load(Ordering::Acquire)
                                })
                        })
                    })
            });
        }
        if entries.len() >= 256 {
            return Err(TokenAcquisitionError::Capacity);
        }
        let entry = Arc::new(Entry::new());
        entries.insert(partition.to_owned(), entry.clone());
        Ok(entry)
    }

    async fn issue(
        &self,
        spec: &TokenResponseSpec,
        port: &dyn CredentialTokenIssuer,
    ) -> Result<AcquiredCredentialToken, TokenAcquisitionError> {
        if spec.refresh_policy != RefreshTokenPolicy::NotExpected && !port.durable_publication() {
            return Err(TokenAcquisitionError::PublicationFailed);
        }
        if let Some(recovered) = port.begin().await? {
            port.authorize().await?;
            if recovered
                .expires_at_unix_ms
                .is_some_and(|at| at <= self.clock.now_ms().saturating_add(spec.refresh_margin_ms))
            {
                return Err(TokenAcquisitionError::InvalidExpiry);
            }
            return Ok(recovered);
        }
        let response = match port.exchange().await {
            Ok(v) => v,
            Err(e) => return Err(port.failed(e).await),
        };
        let may_have_issued = (200..300).contains(&response.status) || response.status >= 500;
        let issued = match decode_token_response(spec, response, self.clock.now_ms()) {
            Ok(v) => v,
            Err(e) => {
                let e = if spec.refresh_policy != RefreshTokenPolicy::NotExpected
                    && may_have_issued
                    && e != TokenAcquisitionError::NeedsAuthorization
                {
                    TokenAcquisitionError::OutcomeUnknown
                } else {
                    e
                };
                return Err(port.failed(e).await);
            }
        };
        let expected_token = issued.access_token.clone();
        let expected_fields = issued.additional_fields.clone();
        let issuer_expiry = issued.access_expires_at_unix_ms;
        if let Err(e) = port.authorize().await {
            return Err(port.failed(e).await);
        }
        let mut published = match port.publish(issued).await {
            Ok(v) => v,
            Err(e) => return Err(port.failed(e).await),
        };
        if published.additional_fields.len() != expected_fields.len()
            || expected_fields.iter().any(|(k, v)| {
                published
                    .additional_fields
                    .get(k)
                    .is_none_or(|a| a.expose_secret() != v.expose_secret())
            })
            || published.access_token.expose_secret() != expected_token.expose_secret()
            || (spec.refresh_policy != RefreshTokenPolicy::NotExpected
                && published.reference.is_none())
        {
            return Err(port.failed(TokenAcquisitionError::PublicationFailed).await);
        }
        published.expires_at_unix_ms =
            issuer_expiry.map(|at| published.expires_at_unix_ms.map_or(at, |cap| at.min(cap)));
        // Publication may change the exact source reference. The owning port
        // validates its published successor, never a read-latest substitute.
        port.authorize().await?;
        Ok(published)
    }
}

#[async_trait]
impl CredentialTokenAcquisition for TokenAcquisitionCoordinator {
    async fn acquire(
        &self,
        partition: Option<&str>,
        spec: &TokenResponseSpec,
        port: &dyn CredentialTokenIssuer,
    ) -> Result<AcquiredCredentialToken, TokenAcquisitionError> {
        spec.validate()?;
        port.authorize().await?;
        let Some(partition) = partition else {
            return self.issue(spec, port).await;
        };
        if partition.is_empty() || partition.len() > 4096 {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        let encoded =
            serde_json::to_vec(spec).map_err(|_| TokenAcquisitionError::InvalidContract)?;
        let fingerprint = Sha256::digest(encoded)
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let key = format!("{}:{partition}:{fingerprint}", partition.len());
        let entry = self.entry(&key)?;
        let mut cached = entry.value.lock().await;
        // A waiter may have outlived the original Run/lease or a source may
        // have been revoked while another request performed the exchange.
        port.authorize().await?;
        let now = self.clock.now_ms();
        if let Some(token) = cached.as_ref()
            && token
                .expires_at_unix_ms
                .is_some_and(|at| at > now.saturating_add(spec.refresh_margin_ms))
            && token
                .cache_generation
                .is_some_and(|g| g > entry.invalidated_through.load(Ordering::Acquire))
        {
            return Ok(token.clone());
        }
        *cached = None;
        let mut token = self.issue(spec, port).await?;
        let generation = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
            .map_err(|_| TokenAcquisitionError::Capacity)?
            + 1;
        token.cache_generation = Some(generation);
        token.cache_key = Some(key);
        token.cache_partition = Some(partition.to_owned());
        if token
            .expires_at_unix_ms
            .is_some_and(|at| at > self.clock.now_ms().saturating_add(spec.refresh_margin_ms))
        {
            *cached = Some(token.clone());
        }
        Ok(token)
    }
    fn invalidate(&self, partition: &str, expected: &AcquiredCredentialToken) {
        if expected.cache_partition.as_deref() != Some(partition) {
            return;
        }
        let Some(generation) = expected.cache_generation else {
            return;
        };
        let Some(key) = expected.cache_key.as_ref() else {
            return;
        };
        let entries = self.entries.lock();
        if let Some(entry) = entries.get(key) {
            entry
                .invalidated_through
                .fetch_max(generation, Ordering::AcqRel);
        }
    }
}

fn text(body: &serde_json::Value, pointer: &str) -> Option<RedactedString> {
    body.pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty() && v.len() <= 16384 && !v.bytes().any(|v| v.is_ascii_control()))
        .map(|v| RedactedString::new(v.to_owned()))
}
fn expiry(
    body: &serde_json::Value,
    pointer: Option<&str>,
    required: bool,
    max: u64,
    unit: awaken_credential_contract::TokenExpiryUnit,
    now: u64,
) -> Result<Option<u64>, TokenAcquisitionError> {
    let Some(pointer) = pointer else {
        return Ok(None);
    };
    let Some(value) = body.pointer(pointer).filter(|v| !v.is_null()) else {
        return if required {
            Err(TokenAcquisitionError::InvalidExpiry)
        } else {
            Ok(None)
        };
    };
    let ttl = value
        .as_u64()
        .or_else(|| {
            let s = value.as_str()?;
            if s.is_empty() || s.len() > 20 || !s.bytes().all(|v| v.is_ascii_digit()) {
                return None;
            }
            s.parse().ok()
        })
        .ok_or(TokenAcquisitionError::InvalidExpiry)?;
    let ttl_ms = match unit {
        awaken_credential_contract::TokenExpiryUnit::Seconds => ttl.checked_mul(1000),
        awaken_credential_contract::TokenExpiryUnit::Milliseconds => Some(ttl),
    }
    .filter(|v| *v > 0 && *v <= max.saturating_mul(1000))
    .ok_or(TokenAcquisitionError::InvalidExpiry)?;
    Ok(Some(
        now.checked_add(ttl_ms)
            .ok_or(TokenAcquisitionError::InvalidExpiry)?,
    ))
}

/// Decode only declared paths. Arbitrary issuer response text never becomes an
/// error or a public receipt. This is the sole response-admission implementation.
pub fn decode_token_response(
    spec: &TokenResponseSpec,
    response: TokenEndpointResponse,
    now: u64,
) -> Result<IssuedCredentialTokens, TokenAcquisitionError> {
    spec.validate()?;
    if spec
        .authorization_required
        .iter()
        .any(|c| response.body.pointer(&c.pointer) == Some(&c.equals))
    {
        return Err(TokenAcquisitionError::NeedsAuthorization);
    }
    if !(200..300).contains(&response.status)
        || spec
            .success
            .iter()
            .any(|c| response.body.pointer(&c.pointer) != Some(&c.equals))
    {
        return Err(TokenAcquisitionError::IssuerRejected);
    }
    let access_token = text(&response.body, &spec.access_token_pointer)
        .ok_or(TokenAcquisitionError::InvalidAccessToken)?;
    let refresh_token = spec
        .refresh_token_pointer
        .as_deref()
        .and_then(|p| text(&response.body, p));
    if spec.refresh_policy == RefreshTokenPolicy::ReplaceRequired && refresh_token.is_none() {
        return Err(TokenAcquisitionError::InvalidRefreshToken);
    }
    if let Some(pointer) = spec.refresh_token_pointer.as_ref()
        && response.body.pointer(pointer).is_some_and(|v| !v.is_null())
        && refresh_token.is_none()
    {
        return Err(TokenAcquisitionError::InvalidRefreshToken);
    }
    let access_expires_at_unix_ms = expiry(
        &response.body,
        spec.access_ttl_pointer.as_deref(),
        spec.require_access_ttl,
        spec.max_access_ttl_seconds,
        spec.access_ttl_unit,
        now,
    )?;
    if access_expires_at_unix_ms.is_some_and(|at| at <= now.saturating_add(spec.refresh_margin_ms))
    {
        return Err(TokenAcquisitionError::InvalidExpiry);
    }
    let refresh_expires_at_unix_ms = expiry(
        &response.body,
        spec.refresh_ttl_pointer.as_deref(),
        spec.require_refresh_ttl,
        spec.max_refresh_ttl_seconds,
        awaken_credential_contract::TokenExpiryUnit::Seconds,
        now,
    )?;
    let additional_fields = spec
        .additional_fields
        .iter()
        .map(|(field, pointer)| {
            text(&response.body, pointer)
                .map(|v| (field.clone(), v))
                .ok_or(TokenAcquisitionError::InvalidAccessToken)
        })
        .collect::<Result<_, _>>()?;
    Ok(IssuedCredentialTokens {
        additional_fields,
        access_token,
        refresh_token,
        access_expires_at_unix_ms,
        refresh_expires_at_unix_ms,
    })
}

#[cfg(test)]
mod tests;
