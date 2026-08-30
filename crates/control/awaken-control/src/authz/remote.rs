use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use awaken_agent_contract::{RedactedString, RedactedStringSource};
use awaken_iam_contract::{
    AuthorizationDecision, AuthorizationRequest, PrincipalRef, ScopeRef, Timestamp,
};
use awaken_iam_host::{AuthReject, HostConfig, IamClient, IamGate, connect_remote};
use base64::Engine as _;

use super::off_event_loop;
use super::{ActionNamespace, now_rfc3339, now_unix, qualified_action};

pub(super) fn cloud_authorization_denial_detail(
    action: &awaken_iam_contract::ActionKey,
    workspace: &str,
) -> String {
    format!(
        "cloud IAM denied action '{}' at Workspace '{}'",
        action.0, workspace
    )
}

#[derive(Clone)]
struct RemoteGateConfig {
    base_url: String,
    audience: String,
    issuer: String,
    service_token: Option<String>,
    service_token_file: Option<PathBuf>,
}

struct RemoteGateState {
    gate: IamGate,
    /// Set only when the interactive token also carries PDP requests.
    user_carrier_token: Option<RedactedString>,
}

/// Remote awaken-iam relying-party adapter for both local Cloud login and a
/// hosted Management workload. User and workload credentials remain distinct:
/// the former authenticates the request, while the latter carries PDP calls.
pub struct RemoteManagementAuthz {
    gate: RwLock<RemoteGateState>,
    user_token_source: Option<Arc<RedactedStringSource>>,
    config: RemoteGateConfig,
}

/// Authenticated request identity together with the credential attributes a
/// route-specific PEP needs. `IamGate` has already verified the JWT signature,
/// issuer, audience and expiry before these claims are decoded; the decoded
/// payload is therefore authorization context, never an independent trust
/// decision.
#[derive(Debug, Clone)]
pub(super) struct RemoteAuthenticatedCredential {
    pub(super) principal: PrincipalRef,
    pub(super) access_token_claims: Option<awaken_iam_host::AccessTokenClaims>,
}

/// Closed failure vocabulary for the synchronous remote-IAM authentication
/// boundary. Transport failure is distinct from a rejected credential so the
/// PEP can retain stable operator diagnostics without exposing IAM internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteAuthenticationFailure {
    Expired,
    Revoked,
    Invalid,
    Transport,
}

pub(super) async fn authenticate_off_event_loop(
    authz: Arc<RemoteManagementAuthz>,
    presented: Option<String>,
) -> Result<RemoteAuthenticatedCredential, RemoteAuthenticationFailure> {
    // Desktop OAuth refresh and remote token verification use synchronous IAM
    // transports. A stale cached token may refresh before authentication.
    match off_event_loop::run(move || authz.authenticate(presented)).await {
        Ok(Ok(authenticated)) => Ok(authenticated),
        Ok(Err(AuthReject::Expired)) => Err(RemoteAuthenticationFailure::Expired),
        Ok(Err(AuthReject::Revoked)) => Err(RemoteAuthenticationFailure::Revoked),
        Ok(Err(AuthReject::Invalid)) => Err(RemoteAuthenticationFailure::Invalid),
        Err(_) => Err(RemoteAuthenticationFailure::Transport),
    }
}

impl RemoteAuthenticatedCredential {
    pub(super) fn is_managed_tunnel_workload(&self) -> bool {
        self.access_token_claims.as_ref().is_some_and(|claims| {
            claims.subject_kind == awaken_iam_host::AccessTokenSubjectKind::Service
                && claims
                    .scope
                    .iter()
                    .any(|scope| scope == "workspace:manage_tunnels")
        })
    }
}

impl RemoteManagementAuthz {
    /// Connect a local interactive process. Its request-time login is the
    /// default request bearer and, absent an explicit service credential, the
    /// PDP carrier. A changed carrier rebuilds the remote gate exactly once.
    pub fn connect_with_user_token_source(
        base_url: String,
        audience: String,
        issuer: String,
        user_token_source: Arc<RedactedStringSource>,
        service_token: Option<String>,
    ) -> Result<Arc<Self>, String> {
        let config = RemoteGateConfig {
            base_url,
            audience,
            issuer,
            service_token,
            service_token_file: None,
        };
        let user_token = resolve_user_token(user_token_source.as_ref())?;
        Self::connect_config(config, Some(user_token_source), Some(user_token))
    }

    /// Connect hosted Management with its projected workload credential. User
    /// authentication must come from each request bearer.
    pub fn connect_with_projected_service_token(
        base_url: String,
        audience: String,
        issuer: String,
        service_token_file: PathBuf,
    ) -> Result<Arc<Self>, String> {
        Self::connect_config(
            RemoteGateConfig {
                base_url,
                audience,
                issuer,
                service_token: None,
                service_token_file: Some(service_token_file),
            },
            None,
            None,
        )
    }

    fn connect_config(
        config: RemoteGateConfig,
        user_token_source: Option<Arc<RedactedStringSource>>,
        user_token: Option<RedactedString>,
    ) -> Result<Arc<Self>, String> {
        let user_is_carrier = config.service_token.is_none()
            && config.service_token_file.is_none()
            && user_token.is_some();
        let gate = build_gate(&config, user_token.as_ref())?;
        Ok(Arc::new(Self {
            gate: RwLock::new(RemoteGateState {
                gate,
                user_carrier_token: if user_is_carrier { user_token } else { None },
            }),
            user_token_source,
            config,
        }))
    }

    /// Return the current interactive Cloud credential only to trusted clients
    /// assembled in this process. Hosted Management has no cached user and
    /// therefore cannot accidentally broker inference as an end user.
    pub fn cloud_user_token(&self) -> Result<Option<RedactedString>, String> {
        self.user_token_source
            .as_deref()
            .map(resolve_user_token)
            .transpose()
    }

    pub(super) fn authenticate(
        &self,
        presented: Option<String>,
    ) -> Result<RemoteAuthenticatedCredential, AuthReject> {
        let sourced;
        let token = match presented.as_deref() {
            Some(token) => token,
            None => {
                sourced = self
                    .cloud_user_token()
                    .map_err(|_| AuthReject::Invalid)?
                    .ok_or(AuthReject::Invalid)?;
                sourced.expose_secret()
            }
        };
        let principal = self
            .gate
            .read()
            .map_err(|_| AuthReject::Invalid)?
            .gate
            .authenticate_detailed(token, &Timestamp(now_rfc3339()), now_unix())
            .map(|(principal, _)| principal)?;
        let access_token_claims = if is_api_token(token) {
            None
        } else {
            Some(decode_verified_access_token_claims(token)?)
        };
        Ok(RemoteAuthenticatedCredential {
            principal,
            access_token_claims,
        })
    }

    pub(super) fn authorize_action(
        &self,
        principal: PrincipalRef,
        action: &str,
        scope: ScopeRef,
        namespace: ActionNamespace,
    ) -> AuthorizationDecision {
        let Ok(gate) = self.gate_for_authorization() else {
            return AuthorizationDecision::Deny;
        };
        IamClient::authorize(
            &gate,
            AuthorizationRequest::direct(principal, qualified_action(namespace, action), scope),
        )
    }

    fn gate_for_authorization(&self) -> Result<IamGate, String> {
        let source = {
            let state = self
                .gate
                .read()
                .map_err(|_| "Cloud IAM gate lock is poisoned".to_string())?;
            if state.user_carrier_token.is_none() {
                return Ok(state.gate.clone());
            }
            self.user_token_source
                .as_deref()
                .ok_or_else(|| "Cloud IAM user-token source is missing".to_string())?
        };
        let observed = resolve_user_token(source)?;
        {
            let state = self
                .gate
                .read()
                .map_err(|_| "Cloud IAM gate lock is poisoned".to_string())?;
            if state
                .user_carrier_token
                .as_ref()
                .is_some_and(|known| known.expose_secret() == observed.expose_secret())
            {
                return Ok(state.gate.clone());
            }
        }

        let mut state = self
            .gate
            .write()
            .map_err(|_| "Cloud IAM gate lock is poisoned".to_string())?;
        // Re-read after acquiring the write lock so concurrent rotations can
        // never overwrite a newer carrier with an earlier observation.
        let current = resolve_user_token(source)?;
        if state
            .user_carrier_token
            .as_ref()
            .is_some_and(|known| known.expose_secret() == current.expose_secret())
        {
            return Ok(state.gate.clone());
        }
        state.gate = build_gate(&self.config, Some(&current))?;
        state.user_carrier_token = Some(current);
        Ok(state.gate.clone())
    }
}

fn is_api_token(token: &str) -> bool {
    token.starts_with("sk-awaken-") || token.starts_with("sk-ant-")
}

fn decode_verified_access_token_claims(
    token: &str,
) -> Result<awaken_iam_host::AccessTokenClaims, AuthReject> {
    let payload = token.split('.').nth(1).ok_or(AuthReject::Invalid)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AuthReject::Invalid)?;
    serde_json::from_slice(&decoded).map_err(|_| AuthReject::Invalid)
}

fn resolve_user_token(source: &RedactedStringSource) -> Result<RedactedString, String> {
    let token = source()?;
    if token.is_empty() {
        return Err("Cloud user-token source returned an empty credential".into());
    }
    Ok(token)
}

fn build_gate(
    config: &RemoteGateConfig,
    user_token: Option<&RedactedString>,
) -> Result<IamGate, String> {
    let mut host = HostConfig::remote(config.base_url.clone())
        .with_audience(config.audience.clone())
        .with_issuer(config.issuer.clone());
    host.service_token = config
        .service_token
        .clone()
        .or_else(|| user_token.map(|token| token.expose_secret().to_owned()));
    host.service_token_file = config.service_token_file.clone();
    // JWKS establishment uses reqwest's blocking client. Keep that private
    // runtime off the async composition thread; request-time PDP calls are
    // already isolated by the management PEP.
    std::thread::spawn(move || connect_remote(&host))
        .join()
        .map_err(|_| "cloud IAM connection worker panicked".to_string())?
        .map(|handle| handle.gate)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
