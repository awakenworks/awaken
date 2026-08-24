//! Coordinator-owned short-lived application credential authority.
//!
//! One durable repository backs mint, authentication, and revocation across
//! Coordinator replicas. The cleartext credential is returned exactly once;
//! only its high-entropy token hash and its narrow application grant persist.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_authz_enforce::{
    ApplicationAccessAuthenticator, ApplicationAuthenticationError, ApplicationGrant,
    ApplicationIdentity,
};
use awaken_iam_core::{EntropySource, OsEntropy, hash_session_token};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use uuid::{Uuid, Version};

mod postgres;
mod schema;
mod sqlite;

use postgres::PostgresApplicationAccessRepository;
use sqlite::SqliteApplicationAccessRepository;

const TOKEN_ID_PREFIX: &str = "aat_";
const TOKEN_SECRET_BYTES: usize = 32;
const TOKEN_HASH_BYTES_BASE64URL: usize = 43;

/// Typed failures at the sole application-access persistence boundary.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ApplicationAccessRepositoryError {
    #[error("invalid application access record: {0}")]
    Invalid(String),
    #[error("application access record not found")]
    NotFound,
    #[error("application access repository unavailable: {0}")]
    Unavailable(String),
    #[error("corrupt application access record: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ApplicationAccessTokenId(Uuid);

impl ApplicationAccessTokenId {
    fn new() -> Self {
        Self(Uuid::now_v7())
    }

    fn parse(value: &str) -> Result<Self, String> {
        let raw = value
            .strip_prefix(TOKEN_ID_PREFIX)
            .ok_or_else(|| "token id has no aat_ prefix".to_string())?;
        let id = Uuid::parse_str(raw).map_err(|error| format!("invalid token UUID: {error}"))?;
        if id.get_version() != Some(Version::SortRand) {
            return Err("token id is not UUIDv7".into());
        }
        Ok(Self(id))
    }
}

impl fmt::Display for ApplicationAccessTokenId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{TOKEN_ID_PREFIX}{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApplicationAccessTokenHash(String);

impl ApplicationAccessTokenHash {
    fn from_presented(presented: &str) -> Self {
        Self(hash_session_token(presented))
    }

    fn parse(value: String) -> Result<Self, String> {
        if value.len() != TOKEN_HASH_BYTES_BASE64URL
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("token hash is not a SHA-256 base64url value".into());
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// One durable application capability. It deliberately contains no cleartext
/// credential, wire-only caller correlation, IAM role, service principal, or
/// Session metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApplicationAccessRecord {
    id: ApplicationAccessTokenId,
    token_hash: ApplicationAccessTokenHash,
    workspace_id: String,
    created_at_unix_ms: u64,
    expires_at_unix_ms: u64,
    revoked_at_unix_ms: Option<u64>,
    grant: ApplicationGrant,
}

impl ApplicationAccessRecord {
    fn new(
        id: ApplicationAccessTokenId,
        token_hash: ApplicationAccessTokenHash,
        workspace_id: String,
        created_at_unix_ms: u64,
        expires_at_unix_ms: u64,
        grant: ApplicationGrant,
    ) -> Result<Self, ApplicationAccessRepositoryError> {
        if workspace_id.trim().is_empty() {
            return Err(ApplicationAccessRepositoryError::Invalid(
                "workspace_id is empty".into(),
            ));
        }
        if expires_at_unix_ms <= created_at_unix_ms {
            return Err(ApplicationAccessRepositoryError::Invalid(
                "expiration is not after creation".into(),
            ));
        }
        for (name, value) in [
            ("created_at_unix_ms", created_at_unix_ms),
            ("expires_at_unix_ms", expires_at_unix_ms),
        ] {
            i64::try_from(value).map_err(|_| {
                ApplicationAccessRepositoryError::Invalid(format!("{name} exceeds i64 storage"))
            })?;
        }
        grant.validate().map_err(|detail| {
            ApplicationAccessRepositoryError::Invalid(format!("invalid grant: {detail}"))
        })?;
        Ok(Self {
            id,
            token_hash,
            workspace_id,
            created_at_unix_ms,
            expires_at_unix_ms,
            revoked_at_unix_ms: None,
            grant,
        })
    }

    fn from_storage(
        row: StoredApplicationAccessRow,
    ) -> Result<Self, ApplicationAccessRepositoryError> {
        let corrupt = |detail: String| ApplicationAccessRepositoryError::Corrupt(detail);
        let id = ApplicationAccessTokenId::parse(&row.token_id).map_err(corrupt)?;
        let token_hash = ApplicationAccessTokenHash::parse(row.token_hash).map_err(corrupt)?;
        if row.workspace_id.trim().is_empty() {
            return Err(corrupt("workspace_id is empty".into()));
        }
        let created_at_unix_ms = u64::try_from(row.created_at_unix_ms)
            .map_err(|_| corrupt("created_at_unix_ms is negative".into()))?;
        let expires_at_unix_ms = u64::try_from(row.expires_at_unix_ms)
            .map_err(|_| corrupt("expires_at_unix_ms is negative".into()))?;
        if expires_at_unix_ms <= created_at_unix_ms {
            return Err(corrupt("expiration is not after creation".into()));
        }
        let revoked_at_unix_ms = row
            .revoked_at_unix_ms
            .map(|value| {
                u64::try_from(value).map_err(|_| corrupt("revoked_at_unix_ms is negative".into()))
            })
            .transpose()?;
        if revoked_at_unix_ms.is_some_and(|revoked| revoked < created_at_unix_ms) {
            return Err(corrupt("revocation precedes creation".into()));
        }
        let grant: ApplicationGrant = serde_json::from_str(&row.grant_json)
            .map_err(|error| corrupt(format!("decode grant: {error}")))?;
        grant
            .validate()
            .map_err(|detail| corrupt(format!("invalid grant: {detail}")))?;
        Ok(Self {
            id,
            token_hash,
            workspace_id: row.workspace_id,
            created_at_unix_ms,
            expires_at_unix_ms,
            revoked_at_unix_ms,
            grant,
        })
    }

    fn stored_row(&self) -> Result<StoredApplicationAccessRow, ApplicationAccessRepositoryError> {
        let timestamp = |name: &str, value: u64| {
            i64::try_from(value).map_err(|_| {
                ApplicationAccessRepositoryError::Invalid(format!("{name} exceeds i64 storage"))
            })
        };
        Ok(StoredApplicationAccessRow {
            token_id: self.id.to_string(),
            token_hash: self.token_hash.0.clone(),
            workspace_id: self.workspace_id.clone(),
            created_at_unix_ms: timestamp("created_at_unix_ms", self.created_at_unix_ms)?,
            expires_at_unix_ms: timestamp("expires_at_unix_ms", self.expires_at_unix_ms)?,
            revoked_at_unix_ms: self
                .revoked_at_unix_ms
                .map(|value| timestamp("revoked_at_unix_ms", value))
                .transpose()?,
            grant_json: serde_json::to_string(&self.grant).map_err(|error| {
                ApplicationAccessRepositoryError::Invalid(format!("encode grant: {error}"))
            })?,
        })
    }
}

struct StoredApplicationAccessRow {
    token_id: String,
    token_hash: String,
    workspace_id: String,
    created_at_unix_ms: i64,
    expires_at_unix_ms: i64,
    revoked_at_unix_ms: Option<i64>,
    grant_json: String,
}

#[async_trait]
trait ApplicationAccessRepository: Send + Sync {
    async fn create(
        &self,
        record: ApplicationAccessRecord,
    ) -> Result<(), ApplicationAccessRepositoryError>;

    async fn find_by_token_hash(
        &self,
        token_hash: &ApplicationAccessTokenHash,
    ) -> Result<ApplicationAccessRecord, ApplicationAccessRepositoryError>;

    async fn revoke(
        &self,
        workspace_id: &str,
        id: &ApplicationAccessTokenId,
        revoked_at_unix_ms: u64,
    ) -> Result<(), ApplicationAccessRepositoryError>;

    /// Delete at most `limit` credentials that became terminal strictly before
    /// the supplied retention boundary. A credential is terminal at expiry or
    /// revocation, whichever happens first.
    async fn delete_terminal_before(
        &self,
        terminal_before_unix_ms: u64,
        limit: u32,
    ) -> Result<u64, ApplicationAccessRepositoryError>;
}

pub(crate) struct MintedApplicationAccess {
    pub(crate) id: String,
    pub(crate) access_token: String,
}

/// The Coordinator's one application credential authority.
pub struct ApplicationAccessStore {
    repository: Arc<dyn ApplicationAccessRepository>,
}

impl ApplicationAccessStore {
    /// Open the application-access schema in the same SQLite file selected for
    /// the Coordinator Session aggregate.
    pub async fn open_sqlite(path: &str) -> Result<Self, ApplicationAccessRepositoryError> {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            SqliteApplicationAccessRepository::open(&path).map(Self::over)
        })
        .await
        .map_err(|error| {
            ApplicationAccessRepositoryError::Unavailable(format!(
                "join application access SQLite open: {error}"
            ))
        })?
    }

    /// Explicit synchronous fixture seam. Production async composition must use
    /// [`Self::open_sqlite`] so open, busy-timeout setup, and migration do not
    /// block a Tokio worker.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_sqlite_for_test(path: &str) -> Result<Self, ApplicationAccessRepositoryError> {
        SqliteApplicationAccessRepository::open(path).map(Self::over)
    }

    /// Connect and migrate the application-access bundle in the Coordinator's
    /// selected PostgreSQL database.
    pub async fn connect_postgres(url: &str) -> Result<Self, ApplicationAccessRepositoryError> {
        PostgresApplicationAccessRepository::connect(url)
            .await
            .map(Self::over)
    }

    /// Apply the application-access bundle during an explicit deployment
    /// migration without constructing a runtime credential authority.
    pub async fn migrate_postgres(url: &str) -> Result<(), ApplicationAccessRepositoryError> {
        PostgresApplicationAccessRepository::migrate(url).await
    }

    /// Connect to an operator-migrated PostgreSQL schema without executing DDL.
    pub async fn connect_existing_postgres(
        url: &str,
    ) -> Result<Self, ApplicationAccessRepositoryError> {
        PostgresApplicationAccessRepository::connect_existing(url)
            .await
            .map(Self::over)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, ApplicationAccessRepositoryError> {
        SqliteApplicationAccessRepository::open_in_memory().map(Self::over)
    }

    fn over(repository: impl ApplicationAccessRepository + 'static) -> Self {
        Self {
            repository: Arc::new(repository),
        }
    }

    #[cfg(test)]
    fn over_shared(repository: Arc<dyn ApplicationAccessRepository>) -> Self {
        Self { repository }
    }

    #[cfg(test)]
    pub(crate) fn failing_for_test(error: ApplicationAccessRepositoryError) -> Self {
        Self::over_shared(Arc::new(FailingApplicationAccessRepository(error)))
    }

    pub(crate) async fn mint(
        &self,
        workspace_id: String,
        created_at_unix_ms: u64,
        expires_at_unix_ms: u64,
        grant: ApplicationGrant,
    ) -> Result<MintedApplicationAccess, ApplicationAccessRepositoryError> {
        let id = ApplicationAccessTokenId::new();
        let access_token = issue_cleartext_token();
        let record = ApplicationAccessRecord::new(
            id.clone(),
            ApplicationAccessTokenHash::from_presented(&access_token),
            workspace_id,
            created_at_unix_ms,
            expires_at_unix_ms,
            grant,
        )?;
        self.repository.create(record).await?;
        Ok(MintedApplicationAccess {
            id: id.to_string(),
            access_token,
        })
    }

    pub(crate) async fn revoke(
        &self,
        workspace_id: &str,
        id: &str,
        revoked_at_unix_ms: u64,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        if workspace_id.trim().is_empty() {
            return Err(ApplicationAccessRepositoryError::Invalid(
                "workspace_id is empty".into(),
            ));
        }
        let id = ApplicationAccessTokenId::parse(id)
            .map_err(ApplicationAccessRepositoryError::Invalid)?;
        self.repository
            .revoke(workspace_id, &id, revoked_at_unix_ms)
            .await
    }

    pub(crate) async fn delete_terminal_before(
        &self,
        terminal_before_unix_ms: u64,
        limit: u32,
    ) -> Result<u64, ApplicationAccessRepositoryError> {
        if limit == 0 {
            return Err(ApplicationAccessRepositoryError::Invalid(
                "application access retention batch is zero".into(),
            ));
        }
        let deleted = self
            .repository
            .delete_terminal_before(terminal_before_unix_ms, limit)
            .await?;
        if deleted > u64::from(limit) {
            return Err(ApplicationAccessRepositoryError::Corrupt(
                "application access retention exceeded its requested batch".into(),
            ));
        }
        Ok(deleted)
    }
}

#[cfg(test)]
struct FailingApplicationAccessRepository(ApplicationAccessRepositoryError);

#[cfg(test)]
#[async_trait]
impl ApplicationAccessRepository for FailingApplicationAccessRepository {
    async fn create(
        &self,
        _record: ApplicationAccessRecord,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        Err(self.0.clone())
    }

    async fn find_by_token_hash(
        &self,
        _token_hash: &ApplicationAccessTokenHash,
    ) -> Result<ApplicationAccessRecord, ApplicationAccessRepositoryError> {
        Err(self.0.clone())
    }

    async fn revoke(
        &self,
        _workspace_id: &str,
        _id: &ApplicationAccessTokenId,
        _revoked_at_unix_ms: u64,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        Err(self.0.clone())
    }

    async fn delete_terminal_before(
        &self,
        _terminal_before_unix_ms: u64,
        _limit: u32,
    ) -> Result<u64, ApplicationAccessRepositoryError> {
        Err(self.0.clone())
    }
}

#[async_trait]
impl ApplicationAccessAuthenticator for ApplicationAccessStore {
    async fn authenticate(
        &self,
        presented: &str,
    ) -> Result<ApplicationIdentity, ApplicationAuthenticationError> {
        let hash = ApplicationAccessTokenHash::from_presented(presented);
        let record = self
            .repository
            .find_by_token_hash(&hash)
            .await
            .map_err(authentication_error)?;
        if record.revoked_at_unix_ms.is_some() || now_unix_millis() >= record.expires_at_unix_ms {
            return Err(ApplicationAuthenticationError::Invalid);
        }
        Ok(ApplicationIdentity {
            workspace_id: record.workspace_id,
            grant: record.grant,
        })
    }
}

fn authentication_error(error: ApplicationAccessRepositoryError) -> ApplicationAuthenticationError {
    match error {
        ApplicationAccessRepositoryError::Invalid(_)
        | ApplicationAccessRepositoryError::NotFound => ApplicationAuthenticationError::Invalid,
        ApplicationAccessRepositoryError::Unavailable(_) => {
            ApplicationAuthenticationError::Unavailable
        }
        ApplicationAccessRepositoryError::Corrupt(_) => ApplicationAuthenticationError::Corrupt,
    }
}

fn issue_cleartext_token() -> String {
    let mut bytes = [0_u8; TOKEN_SECRET_BYTES];
    OsEntropy.fill_bytes(&mut bytes);
    format!("{TOKEN_ID_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
