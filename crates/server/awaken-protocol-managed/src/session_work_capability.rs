//! Official Managed Agents Session Work capability configuration.
//!
//! The WorkQueue remains lease authority and `AccessTokenAuthority` remains key
//! authority. This value only binds their existing facts to the protocol's
//! `WorkSecret.sessions_token`; it owns no credential directory or lifecycle.

use awaken_iam_server::{AccessTokenAuthority, LeaseEpoch, MintCapability, mint_capability};
use awaken_session_contract::work_queue::SessionWorkLease;

/// The one exact scope carried by a Session Work capability.
pub const SESSION_WORK_SCOPE: &str = "managed.session.serve";
/// A capability must outlive the queue's initial lease. Every authorized call
/// additionally checks the current live lease, so a longer JWT window does not
/// extend Work authority.
pub const MIN_SESSION_WORK_CAPABILITY_TTL_SECONDS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionWorkCapabilityConfigurationError {
    #[error("Session Work capability issuer must not be empty")]
    EmptyIssuer,
    #[error("Session Work capability audience must not be empty")]
    EmptyAudience,
    #[error("Session Work capability TTL must be at least 60 seconds")]
    TtlTooShort,
    #[error("Session Work capability time is outside the supported Unix range")]
    TimeRange,
    #[error("Session Work capability signing failed: {0}")]
    Signing(String),
}

/// Immutable issuer/verifier configuration shared by poll projection and the
/// Coordinator capability guard. Clones share the exact signer set and JWKS.
#[derive(Clone)]
pub struct SessionWorkCapabilityConfiguration {
    authority: AccessTokenAuthority,
    issuer: String,
    audience: String,
    ttl_seconds: u64,
}

impl SessionWorkCapabilityConfiguration {
    pub fn new(
        authority: AccessTokenAuthority,
        issuer: impl Into<String>,
        audience: impl Into<String>,
        ttl_seconds: u64,
    ) -> Result<Self, SessionWorkCapabilityConfigurationError> {
        let issuer = issuer.into();
        if issuer.trim().is_empty() {
            return Err(SessionWorkCapabilityConfigurationError::EmptyIssuer);
        }
        let audience = audience.into();
        if audience.trim().is_empty() {
            return Err(SessionWorkCapabilityConfigurationError::EmptyAudience);
        }
        if ttl_seconds < MIN_SESSION_WORK_CAPABILITY_TTL_SECONDS {
            return Err(SessionWorkCapabilityConfigurationError::TtlTooShort);
        }
        Ok(Self {
            authority,
            issuer,
            audience,
            ttl_seconds,
        })
    }

    #[must_use]
    pub fn authority(&self) -> &AccessTokenAuthority {
        &self.authority
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    pub async fn mint(
        &self,
        lease: &SessionWorkLease,
        now_ms: u64,
    ) -> Result<String, SessionWorkCapabilityConfigurationError> {
        let iat = i64::try_from(now_ms / 1_000)
            .map_err(|_| SessionWorkCapabilityConfigurationError::TimeRange)?;
        let ttl = i64::try_from(self.ttl_seconds)
            .map_err(|_| SessionWorkCapabilityConfigurationError::TimeRange)?;
        let exp = iat
            .checked_add(ttl)
            .ok_or(SessionWorkCapabilityConfigurationError::TimeRange)?;
        mint_capability(
            &self.authority,
            MintCapability {
                iss: self.issuer.clone(),
                sub: lease.session_id.clone(),
                aud: self.audience.clone(),
                jti: uuid::Uuid::now_v7().to_string(),
                iat,
                exp,
                epoch: LeaseEpoch(lease.epoch),
                scope: vec![SESSION_WORK_SCOPE.to_string()],
                obligation: None,
            },
        )
        .await
        .map_err(|error| SessionWorkCapabilityConfigurationError::Signing(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use awaken_iam_server::{CapabilityCheck, LocalSeedSigner, verify_capability};

    use super::*;

    fn authority() -> AccessTokenAuthority {
        AccessTokenAuthority::new(LocalSeedSigner::new("session-work-test", [0x51; 32]))
    }

    #[tokio::test]
    async fn configuration_and_minting_preserve_every_capability_fence() {
        // Cause/effect graph: C1 issuer present; C2 audience present; C3 TTL at
        // least the initial lease; C4 exact lease epoch/session; C5 Unix time is
        // representable. Effects: E1 invalid configuration rejects before
        // startup, E2 a valid token has one exact scope/subject/epoch and bounded
        // expiry, E3 overflowing time fails before signing.
        //
        // | Rule | issuer | audience | TTL/time | effect |
        // | P1 | empty | valid | valid | EmptyIssuer |
        // | P2 | valid | empty | valid | EmptyAudience |
        // | P3 | valid | valid | <60 | TtlTooShort |
        // | P4 | valid | valid | bounded | exact signed claims |
        // | P5 | valid | valid | unrepresentable TTL | TimeRange |
        assert!(
            matches!(
                SessionWorkCapabilityConfiguration::new(authority(), " ", "aud", 60),
                Err(SessionWorkCapabilityConfigurationError::EmptyIssuer)
            ),
            "P1/E1"
        );
        assert!(
            matches!(
                SessionWorkCapabilityConfiguration::new(authority(), "iss", " ", 60),
                Err(SessionWorkCapabilityConfigurationError::EmptyAudience)
            ),
            "P2/E1"
        );
        assert!(
            matches!(
                SessionWorkCapabilityConfiguration::new(authority(), "iss", "aud", 59),
                Err(SessionWorkCapabilityConfigurationError::TtlTooShort)
            ),
            "P3/E1"
        );

        let config = SessionWorkCapabilityConfiguration::new(
            authority(),
            "urn:test:issuer",
            "managed-worker",
            120,
        )
        .unwrap();
        let lease = SessionWorkLease {
            work_id: "work-1".into(),
            environment_id: "env-1".into(),
            session_id: "session-1".into(),
            owner: "owner-1".into(),
            epoch: 7,
            expires_at_unix_ms: 70_000,
        };
        let token = config.mint(&lease, 10_000).await.expect("P4 mint");
        let claims = verify_capability(
            &token,
            &config.authority().jwks(),
            CapabilityCheck {
                audience: "managed-worker",
                epoch: LeaseEpoch(7),
                now: 11,
            },
        )
        .expect("P4 verify");
        assert_eq!(claims.iss, "urn:test:issuer", "P4/E2");
        assert_eq!(claims.sub, "session-1", "P4/E2");
        assert_eq!(claims.scope, [SESSION_WORK_SCOPE], "P4/E2");
        assert_eq!((claims.iat, claims.exp), (10, 130), "P4/E2");

        let overflow =
            SessionWorkCapabilityConfiguration::new(authority(), "iss", "aud", u64::MAX).unwrap();
        assert!(
            matches!(
                overflow.mint(&lease, 0).await,
                Err(SessionWorkCapabilityConfigurationError::TimeRange)
            ),
            "P5/E3"
        );
    }
}
