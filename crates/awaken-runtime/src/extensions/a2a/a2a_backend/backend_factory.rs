use super::{A2A_BACKEND, A2aBackend, A2aBackendFactory, A2aConfig};
use crate::backend::{
    ExecutionBackend, ExecutionBackendConfigSchema, ExecutionBackendFactory,
    ExecutionBackendFactoryError,
};
use awaken_runtime_contract::registry_spec::RemoteEndpoint;
use std::sync::Arc;

impl ExecutionBackendFactory for A2aBackendFactory {
    fn backend(&self) -> &str {
        A2A_BACKEND
    }

    fn config_schema(&self) -> ExecutionBackendConfigSchema {
        super::super::config_schema::a2a_backend_config_schema()
    }

    fn validate(&self, endpoint: &RemoteEndpoint) -> Result<(), ExecutionBackendFactoryError> {
        A2aConfig::try_from_remote_endpoint(endpoint)
            .map(|_| ())
            .map_err(|error| ExecutionBackendFactoryError::InvalidConfig(error.to_string()))
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Arc<dyn ExecutionBackend>, ExecutionBackendFactoryError> {
        let config = A2aConfig::try_from_remote_endpoint(endpoint)
            .map_err(|error| ExecutionBackendFactoryError::InvalidConfig(error.to_string()))?;
        Ok(Arc::new(A2aBackend::new(config)))
    }
}
