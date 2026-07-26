use std::path::PathBuf;
use std::sync::Arc;

use awaken_iam_contract::{
    AuthorizationDecision, AuthorizationRequest, PrincipalRef, ScopeRef, Timestamp,
};
use awaken_iam_host::{AuthReject, HostConfig, IamClient, IamGate, connect_remote};

use super::{now_rfc3339, now_unix, qualify_action, qualify_resource_action};

/// Remote awaken-iam relying-party adapter for both local Cloud login and a
/// hosted Management workload. User and workload credentials remain distinct:
/// the former authenticates the request, while the latter carries PDP calls.
pub struct RemoteManagementAuthz {
    gate: IamGate,
    user_token: Option<String>,
}

impl RemoteManagementAuthz {
    /// Connect a local interactive process. Its cached login is the default
    /// request bearer and, absent an explicit service credential, PDP carrier.
    pub fn connect(
        base_url: String,
        audience: String,
        issuer: String,
        user_token: String,
        service_token: Option<String>,
    ) -> Result<Arc<Self>, String> {
        Self::connect_config(
            base_url,
            audience,
            issuer,
            Some(user_token),
            service_token,
            None,
        )
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
            base_url,
            audience,
            issuer,
            None,
            None,
            Some(service_token_file),
        )
    }

    fn connect_config(
        base_url: String,
        audience: String,
        issuer: String,
        user_token: Option<String>,
        service_token: Option<String>,
        service_token_file: Option<PathBuf>,
    ) -> Result<Arc<Self>, String> {
        let mut config = HostConfig::remote(base_url)
            .with_audience(audience)
            .with_issuer(issuer);
        config.service_token = service_token.or_else(|| user_token.clone());
        config.service_token_file = service_token_file;
        // JWKS establishment uses reqwest's blocking client. Keep that private
        // runtime off the async composition thread; request-time PDP calls are
        // already isolated by the management PEP.
        let handle = std::thread::spawn(move || connect_remote(&config))
            .join()
            .map_err(|_| "cloud IAM connection worker panicked".to_string())?
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            gate: handle.gate,
            user_token,
        }))
    }

    pub(super) fn authenticate(
        &self,
        presented: Option<String>,
    ) -> Result<PrincipalRef, AuthReject> {
        let token = presented
            .as_deref()
            .or(self.user_token.as_deref())
            .ok_or(AuthReject::Invalid)?;
        self.gate
            .authenticate_detailed(token, &Timestamp(now_rfc3339()), now_unix())
            .map(|(principal, _)| principal)
    }

    pub(super) fn authorize(
        &self,
        principal: PrincipalRef,
        action: &str,
        scope: ScopeRef,
    ) -> AuthorizationDecision {
        IamClient::authorize(
            &self.gate,
            AuthorizationRequest::direct(principal, qualify_action(action), scope),
        )
    }

    pub(super) fn authorize_resource(
        &self,
        principal: PrincipalRef,
        action: &str,
        scope: ScopeRef,
    ) -> AuthorizationDecision {
        IamClient::authorize(
            &self.gate,
            AuthorizationRequest::direct(principal, qualify_resource_action(action), scope),
        )
    }
}
