use serde_json::{Map, Value};

use awaken_server_contract::A2aServerSpec;

use crate::services::config_envelope::unwrap_spec;

use super::{ConfigNamespace, ConfigService, ConfigServiceError};

impl ConfigService {
    pub(super) async fn normalize_mcp_server_payload(
        &self,
        path_id: Option<&str>,
        body: &mut Map<String, Value>,
    ) -> Result<(), ConfigServiceError> {
        if body.contains_key("env") || path_id.is_none() {
            return Ok(());
        }

        let Some(path_id) = path_id else {
            return Ok(());
        };
        let Some(existing) = self
            .store
            .get(ConfigNamespace::McpServers.as_str(), path_id)
            .await?
        else {
            return Ok(());
        };
        // The stored value may be either a bare spec or a ConfigRecord envelope.
        let spec_value = unwrap_spec(existing);
        let Some(existing_object) = spec_value.as_object() else {
            return Ok(());
        };
        if let Some(existing_env) = existing_object.get("env") {
            body.insert("env".into(), existing_env.clone());
        }
        Ok(())
    }

    pub(super) async fn normalize_a2a_server_payload(
        &self,
        path_id: Option<&str>,
        id: &str,
        body: &mut Map<String, Value>,
    ) -> Result<(), ConfigServiceError> {
        if body.contains_key("auth") || path_id.is_none() {
            return Ok(());
        }

        let Some(existing) = self
            .store
            .get(ConfigNamespace::A2aServers.as_str(), id)
            .await?
        else {
            return Ok(());
        };
        let spec_value = unwrap_spec(existing);
        let Some(existing_object) = spec_value.as_object() else {
            return Ok(());
        };
        if let Some(existing_auth) = existing_object.get("auth") {
            body.insert("auth".into(), existing_auth.clone());
        }
        Ok(())
    }

    pub async fn a2a_server_spec(
        &self,
        id: &str,
    ) -> Result<Option<A2aServerSpec>, ConfigServiceError> {
        let Some(existing) = self
            .store
            .get(ConfigNamespace::A2aServers.as_str(), id)
            .await?
        else {
            return Ok(None);
        };
        let spec_value = unwrap_spec(existing);
        serde_json::from_value(spec_value)
            .map(Some)
            .map_err(|error| ConfigServiceError::Serialization(error.to_string()))
    }
}
