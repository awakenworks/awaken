//! Authentication primitives shared by private service-to-service adapters.
//!
//! This leaf owns no HTTP framework, identity store, authorization policy, or
//! domain command. Adapters pass raw header bytes after transport parsing.

use std::sync::Arc;

pub const COORDINATOR_SERVICE_AUDIENCE: &str = "awaken-coordinator";
pub const CONTROL_SERVICE_AUDIENCE: &str = "awaken-control";

/// Route-owned facts an injected service authenticator must authorize. The
/// contract deliberately keeps audience and permission values opaque: Cloud IAM
/// may interpret them as JWT claims while local composition can use the static
/// adapter without this leaf depending on either HTTP or an identity provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceAuthorizationRequirement<'a> {
    pub audience: &'a str,
    pub permission: &'a str,
    pub workspace_id: Option<&'a str>,
}

impl<'a> ServiceAuthorizationRequirement<'a> {
    #[must_use]
    pub const fn new(audience: &'a str, permission: &'a str) -> Self {
        Self {
            audience,
            permission,
            workspace_id: None,
        }
    }

    #[must_use]
    pub const fn in_workspace(mut self, workspace_id: &'a str) -> Self {
        self.workspace_id = Some(workspace_id);
        self
    }
}

/// Authenticated workload identity returned before a private domain port is
/// invoked. Route adapters need only the successful decision; composition-owned
/// policies retain responsibility for signature, audience, permission, and
/// workspace checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedService {
    pub principal: Arc<str>,
}

impl AuthenticatedService {
    #[must_use]
    pub fn new(principal: impl Into<Arc<str>>) -> Self {
        Self {
            principal: principal.into(),
        }
    }
}

/// Typed private-boundary failures preserve the distinction between an invalid
/// credential, an authenticated but unauthorized workload, and an unavailable
/// identity dependency. HTTP adapters map these to 401, 403, and 503 before any
/// domain command is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceAuthError {
    Unauthorized,
    Forbidden,
    Unavailable(String),
}

impl std::fmt::Display for ServiceAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => formatter.write_str("private service credential rejected"),
            Self::Forbidden => formatter.write_str("private service operation forbidden"),
            Self::Unavailable(message) => {
                write!(
                    formatter,
                    "private service authentication unavailable: {message}"
                )
            }
        }
    }
}

impl std::error::Error for ServiceAuthError {}

/// Request-time source for a private service bearer.
///
/// Implementations may read a projected file, secret manager, or an atomically
/// replaceable in-memory value. Callers must resolve it for every request so a
/// rotation never requires rebuilding an HTTP router or client.
pub trait ServiceBearerTokenSource: Send + Sync {
    fn current_token(&self) -> Result<Arc<str>, String>;
}

/// Injectable authentication policy for a private service router.
pub trait ServiceRequestAuthenticator: Send + Sync {
    fn authenticate(
        &self,
        authorization: Option<&[u8]>,
        requirement: ServiceAuthorizationRequirement<'_>,
    ) -> Result<AuthenticatedService, ServiceAuthError>;
}

#[derive(Clone)]
pub struct StaticServiceBearerTokenSource(Arc<str>);

impl StaticServiceBearerTokenSource {
    pub fn new(token: impl Into<String>) -> Result<Self, String> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err("private service bearer token must not be empty".into());
        }
        Ok(Self(Arc::from(token)))
    }
}

impl ServiceBearerTokenSource for StaticServiceBearerTokenSource {
    fn current_token(&self) -> Result<Arc<str>, String> {
        Ok(self.0.clone())
    }
}

pub struct TokenSourceAuthenticator {
    source: Arc<dyn ServiceBearerTokenSource>,
}

impl TokenSourceAuthenticator {
    #[must_use]
    pub fn new(source: Arc<dyn ServiceBearerTokenSource>) -> Self {
        Self { source }
    }
}

impl ServiceRequestAuthenticator for TokenSourceAuthenticator {
    fn authenticate(
        &self,
        authorization: Option<&[u8]>,
        _requirement: ServiceAuthorizationRequirement<'_>,
    ) -> Result<AuthenticatedService, ServiceAuthError> {
        let expected = resolve_service_bearer_token(self.source.as_ref())
            .map_err(ServiceAuthError::Unavailable)?;
        service_bearer_token_matches(authorization, &expected)
            .then(|| AuthenticatedService::new("static-service-token"))
            .ok_or(ServiceAuthError::Unauthorized)
    }
}

/// Resolve and validate the credential at the request boundary. Keeping this
/// check here makes every client and router fail closed if a dynamic provider
/// transiently projects an empty token during rotation.
pub fn resolve_service_bearer_token(
    source: &dyn ServiceBearerTokenSource,
) -> Result<Arc<str>, String> {
    let token = source.current_token()?;
    if token.trim().is_empty() {
        return Err("private service bearer token source returned an empty token".into());
    }
    Ok(token)
}

/// Re-read a token after an authentication rejection and report whether a
/// rotation raced the request. Clients may retry only this bounded case; an
/// unchanged rejected credential remains a terminal configuration error.
pub fn service_bearer_token_rotated(
    source: &dyn ServiceBearerTokenSource,
    attempted: &str,
) -> Result<bool, String> {
    Ok(resolve_service_bearer_token(source)?.as_ref() != attempted)
}

pub fn static_token_source(
    token: impl Into<String>,
) -> Result<Arc<dyn ServiceBearerTokenSource>, String> {
    Ok(Arc::new(StaticServiceBearerTokenSource::new(token)?))
}

pub fn static_token_authenticator(
    token: impl Into<String>,
) -> Result<Arc<dyn ServiceRequestAuthenticator>, String> {
    Ok(Arc::new(TokenSourceAuthenticator::new(
        static_token_source(token)?,
    )))
}

/// Match the exact private Bearer credential projected into both sides of one
/// service boundary. Scheme matching is deliberately case-sensitive and the
/// function fails closed for a missing or malformed header.
#[must_use]
pub fn service_bearer_token_matches(authorization: Option<&[u8]>, expected: &str) -> bool {
    authorization
        .and_then(|value| value.strip_prefix(b"Bearer "))
        .is_some_and(|actual| actual == expected.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    #[test]
    fn private_bearer_matching_fails_closed() {
        // Cause/effect decision table: R1 exact scheme and bytes -> authorize;
        // R2 missing header, R3 wrong scheme, R4 wrong token -> deny. These four
        // rules cover both parsing branches and both equality outcomes.
        assert!(
            service_bearer_token_matches(Some(b"Bearer secret"), "secret"),
            "R1"
        );
        assert!(!service_bearer_token_matches(None, "secret"), "R2");
        assert!(
            !service_bearer_token_matches(Some(b"bearer secret"), "secret"),
            "R3"
        );
        assert!(
            !service_bearer_token_matches(Some(b"Bearer other"), "secret"),
            "R4"
        );
    }

    #[test]
    fn injected_authenticator_observes_token_rotation_per_request() {
        // Cause/effect decision table: R1 current token -> allow; R2 missing or
        // wrong token -> deny; R3 source rotates -> old token is denied and the
        // successor is allowed without rebuilding the authenticator; R4 source
        // failure or empty projection -> fail closed with an error. R1/R2 are
        // also covered by the parser table above; this case owns R3/R4.
        struct RotatingSource(RwLock<Result<Arc<str>, String>>);
        impl ServiceBearerTokenSource for RotatingSource {
            fn current_token(&self) -> Result<Arc<str>, String> {
                self.0.read().unwrap().clone()
            }
        }

        let source = Arc::new(RotatingSource(RwLock::new(Ok(Arc::from("first")))));
        let authenticator = TokenSourceAuthenticator::new(source.clone());
        let requirement = ServiceAuthorizationRequirement::new("coordinator", "agent:publish");
        assert_eq!(
            authenticator
                .authenticate(Some(b"Bearer first"), requirement)
                .unwrap()
                .principal
                .as_ref(),
            "static-service-token",
            "R1"
        );
        *source.0.write().unwrap() = Ok(Arc::from("second"));
        assert!(
            service_bearer_token_rotated(source.as_ref(), "first").unwrap(),
            "R3 raced request retries"
        );
        assert!(
            !service_bearer_token_rotated(source.as_ref(), "second").unwrap(),
            "R2 unchanged rejection stays terminal"
        );
        assert_eq!(
            authenticator.authenticate(Some(b"Bearer first"), requirement),
            Err(ServiceAuthError::Unauthorized),
            "R3 old"
        );
        assert!(
            authenticator
                .authenticate(Some(b"Bearer second"), requirement)
                .is_ok(),
            "R3 new"
        );
        *source.0.write().unwrap() = Ok(Arc::from(" "));
        assert!(
            matches!(
                authenticator.authenticate(Some(b"Bearer "), requirement),
                Err(ServiceAuthError::Unavailable(_))
            ),
            "R4 empty"
        );
        *source.0.write().unwrap() = Err("secret backend unavailable".into());
        assert!(
            matches!(
                authenticator.authenticate(Some(b"Bearer second"), requirement),
                Err(ServiceAuthError::Unavailable(_))
            ),
            "R4 error"
        );
    }

    #[test]
    fn typed_authorization_requirement_preserves_route_scope() {
        // Cause/effect decision table: R1 a route without a Workspace carries
        // only audience+permission; R2 a Workspace-owned mutation additionally
        // carries the exact Workspace. These are the complete inputs an injected
        // IAM adapter needs to distinguish 401 from 403 without importing a
        // domain command or HTTP type into this leaf contract.
        let global = ServiceAuthorizationRequirement::new("coordinator", "worker:observe");
        assert_eq!(global.workspace_id, None, "R1");
        let scoped = ServiceAuthorizationRequirement::new("coordinator", "agent:publish")
            .in_workspace("workspace-a");
        assert_eq!(scoped.audience, "coordinator", "R2");
        assert_eq!(scoped.permission, "agent:publish", "R2");
        assert_eq!(scoped.workspace_id, Some("workspace-a"), "R2");
    }
}
