use async_trait::async_trait;
use axum::http::request::Parts;
use thiserror::Error;

use awaken_contract::{RequestSurface, ScopeContext, ScopeId};

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScopeResolveError {
    #[error("scope resolution failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait HttpScopeProvider: Send + Sync {
    async fn scope_for_http_request(
        &self,
        surface: RequestSurface,
        parts: &Parts,
    ) -> Result<ScopeContext, ScopeResolveError>;
}

#[derive(Debug, Clone)]
pub struct SingleScopeProvider {
    scope_id: ScopeId,
}

impl SingleScopeProvider {
    pub fn new(scope_id: ScopeId) -> Self {
        Self { scope_id }
    }

    pub fn default_scope() -> Self {
        Self::new(ScopeId::default_scope())
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }
}

impl Default for SingleScopeProvider {
    fn default() -> Self {
        Self::default_scope()
    }
}

#[async_trait]
impl HttpScopeProvider for SingleScopeProvider {
    async fn scope_for_http_request(
        &self,
        _surface: RequestSurface,
        _parts: &Parts,
    ) -> Result<ScopeContext, ScopeResolveError> {
        Ok(ScopeContext::new(self.scope_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_scope_provider_returns_bound_scope_for_each_surface() {
        let provider = SingleScopeProvider::new(ScopeId::new("scope-a").unwrap());
        let request = axum::http::Request::builder().body(()).unwrap();
        let (parts, _) = request.into_parts();

        let admin = provider
            .scope_for_http_request(RequestSurface::Admin, &parts)
            .await
            .unwrap();
        let invoke = provider
            .scope_for_http_request(RequestSurface::AgentInvoke, &parts)
            .await
            .unwrap();

        assert_eq!(admin.scope_id.as_str(), "scope-a");
        assert_eq!(invoke.scope_id.as_str(), "scope-a");
    }
}
