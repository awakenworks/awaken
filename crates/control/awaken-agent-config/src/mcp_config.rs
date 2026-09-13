//! Workspace MCP authoring. Credentials are references, never owned here.
//! A bounded Workspace configuration owns local definitions, a cloud snapshot
//! and default choices atomically. It does not own Agent or Session bindings.
use awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: Option<String>,
    #[serde(with = "transport_wire")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "transport_wire::McpTransportConfig")
    )]
    pub transport: AgentMcpTransportBinding,
    #[serde(default)]
    pub prompts_as_skills: bool,
}
impl McpServerDefinition {
    pub fn validate(&self) -> Result<(), McpConfigError> {
        if self.id.is_empty()
            || self.id.len() > 80
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err(McpConfigError::Invalid(
                "server id must contain 1-80 ASCII letters, digits, '-' or '_'".into(),
            ));
        }
        if self.name.trim().is_empty() || self.name.len() > 256 || self.description.len() > 4096 {
            return Err(McpConfigError::Invalid(
                "server name/description is empty or too long".into(),
            ));
        }
        if self
            .icon
            .as_ref()
            .is_some_and(|v| v.len() > 2048 || !v.starts_with("https://"))
        {
            return Err(McpConfigError::Invalid("icon must be an HTTPS URL".into()));
        }
        if let Some(icon) = &self.icon {
            awaken_agent_contract::McpTarget::parse_http(icon)
                .map_err(|e| McpConfigError::Invalid(e.to_string()))?;
        }
        self.transport
            .normalize()
            .map_err(McpConfigError::Invalid)?;
        let bytes = serde_json::to_vec(&self.transport)
            .map_err(|e| McpConfigError::Invalid(e.to_string()))?;
        if bytes.len() > 8192 {
            return Err(McpConfigError::Invalid(
                "MCP transport exceeds 8192 bytes".into(),
            ));
        }
        Ok(())
    }
    pub fn target_fingerprint(&self) -> Result<String, McpConfigError> {
        let target = self
            .transport
            .normalize()
            .map_err(McpConfigError::Invalid)?;
        Ok(target.fingerprint().to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpCatalogSnapshot {
    pub schema_version: u32,
    pub revision: u64,
    pub servers: Vec<McpServerDefinition>,
}
impl McpCatalogSnapshot {
    pub fn validate(&self) -> Result<(), McpConfigError> {
        if self.schema_version != 1 || self.revision == 0 || self.servers.len() > 64 {
            return Err(McpConfigError::Invalid(
                "invalid MCP catalog version or size".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for server in &self.servers {
            server.validate()?;
            if !ids.insert(&server.id) {
                return Err(McpConfigError::Invalid(
                    "duplicate catalog server id".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String, McpConfigError> {
        Ok(format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(self).map_err(|e| McpConfigError::Invalid(e.to_string()))?
            )
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpCredentialPreference {
    pub vault_credential_id: String,
    /// A default for an old endpoint cannot silently authorize a replacement.
    pub target_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfiguration {
    pub workspace_id: String,
    pub revision: u64,
    pub local: BTreeMap<String, McpServerDefinition>,
    pub catalog: McpCatalogSnapshot,
    pub defaults: BTreeMap<String, McpCredentialPreference>,
    pub catalog_url: Option<String>,
    pub catalog_source: Option<String>,
}
impl McpConfiguration {
    pub fn new(workspace_id: String, catalog: McpCatalogSnapshot) -> Self {
        Self {
            workspace_id,
            revision: 0,
            local: BTreeMap::new(),
            catalog,
            defaults: BTreeMap::new(),
            catalog_url: None,
            catalog_source: None,
        }
    }
    pub fn server(&self, key: &str) -> Option<&McpServerDefinition> {
        if let Some(id) = key.strip_prefix("local:") {
            self.local.get(id)
        } else {
            key.strip_prefix("catalog:")
                .and_then(|id| self.catalog.servers.iter().find(|s| s.id == id))
        }
    }
    pub fn validate(&self) -> Result<(), McpConfigError> {
        if self.workspace_id.trim().is_empty() || self.local.len() > 64 || self.defaults.len() > 128
        {
            return Err(McpConfigError::Invalid(
                "invalid Workspace MCP configuration size/scope".into(),
            ));
        }
        self.catalog.validate()?;
        for (id, server) in &self.local {
            server.validate()?;
            if id != &server.id {
                return Err(McpConfigError::Invalid(
                    "server identity does not match its key".into(),
                ));
            }
        }
        // Defaults may retain a withdrawn catalog entry for an explicit repair,
        // but must never silently be applied to a different target.
        for (key, value) in &self.defaults {
            if !(key.starts_with("local:") || key.starts_with("catalog:"))
                || value.vault_credential_id.is_empty()
                || value.vault_credential_id.len() > 256
                || value.target_fingerprint.is_empty()
            {
                return Err(McpConfigError::Invalid(
                    "invalid MCP default credential reference".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn advance(&mut self, expected: u64) -> Result<(), McpConfigError> {
        if self.revision != expected {
            return Err(McpConfigError::Conflict);
        }
        self.revision = expected.checked_add(1).ok_or(McpConfigError::Conflict)?;
        Ok(())
    }
    pub fn put_local(&mut self, server: McpServerDefinition) -> Result<(), McpConfigError> {
        server.validate()?;
        let mut next = self.clone();
        next.local.insert(server.id.clone(), server);
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn remove_local(&mut self, id: &str) {
        self.local.remove(id);
        self.defaults.remove(&format!("local:{id}"));
    }
    pub fn apply_catalog(&mut self, snapshot: McpCatalogSnapshot) -> Result<bool, McpConfigError> {
        snapshot.validate()?;
        if self.catalog_url.is_some() && self.catalog_source != self.catalog_url {
            self.catalog = snapshot;
            self.catalog_source = self.catalog_url.clone();
            return Ok(true);
        }
        if snapshot.revision < self.catalog.revision {
            return Err(McpConfigError::Conflict);
        }
        if snapshot.revision == self.catalog.revision {
            if snapshot != self.catalog {
                return Err(McpConfigError::Conflict);
            }
            return Ok(false);
        }
        self.catalog = snapshot;
        Ok(true)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("MCP configuration changed; reload before retrying")]
    Conflict,
    #[error("MCP configuration storage failed: {0}")]
    Storage(String),
}

pub trait McpConfigurationRepository: Send + Sync {
    fn get_mcp_configuration(
        &self,
        workspace: &str,
    ) -> Result<Option<McpConfiguration>, McpConfigError>;
    fn put_mcp_configuration(
        &self,
        expected_revision: u64,
        next: &McpConfiguration,
    ) -> Result<(), McpConfigError>;
    fn mcp_configuration_workspaces(&self) -> Result<Vec<String>, McpConfigError>;
}

/// Same write predicate for every storage backend. The database CAS remains
/// necessary even after this validation; another writer may have committed.
pub fn validate_mcp_configuration_write(
    expected: u64,
    next: &McpConfiguration,
) -> Result<(), McpConfigError> {
    next.validate()?;
    if expected.checked_add(1) != Some(next.revision) {
        return Err(McpConfigError::Conflict);
    }
    Ok(())
}

// Config HTTP DTO is explicit; the domain retains its existing transport value.
// Unknown transport fields (including credential material) never disappear into
// the permissive historical Agent transport decoder.
mod transport_wire {
    use super::*;
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
    #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
    pub(super) enum McpTransportConfig {
        Url {
            url: String,
        },
        SandboxStdio {
            command: String,
            #[serde(default)]
            args: Vec<String>,
        },
    }
    pub fn serialize<S: serde::Serializer>(
        value: &AgentMcpTransportBinding,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            AgentMcpTransportBinding::Http(t) => McpTransportConfig::Url { url: t.url.clone() },
            AgentMcpTransportBinding::SandboxStdio(t) => McpTransportConfig::SandboxStdio {
                command: t.command.clone(),
                args: t.args.clone(),
            },
        }
        .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<AgentMcpTransportBinding, D::Error> {
        Ok(match McpTransportConfig::deserialize(deserializer)? {
            McpTransportConfig::Url { url } => AgentMcpTransportBinding::http(url),
            McpTransportConfig::SandboxStdio { command, args } => {
                AgentMcpTransportBinding::sandbox_stdio(command, args)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn server(id: &str, url: &str) -> McpServerDefinition {
        McpServerDefinition {
            id: id.into(),
            name: id.into(),
            description: "test".into(),
            icon: None,
            transport: AgentMcpTransportBinding::http(url),
            prompts_as_skills: false,
        }
    }
    fn config() -> McpConfiguration {
        McpConfiguration::new(
            "workspace-a".into(),
            McpCatalogSnapshot {
                schema_version: 1,
                revision: 1,
                servers: vec![server("notion", "https://mcp.example.test/mcp")],
            },
        )
    }
    #[test]
    fn definition_wire_and_target_decision_table() {
        // Causes: D1 valid HTTP/stdio shape, D2 invalid endpoint/command,
        // D3 unknown/material field. Effects: accept exact normalized target or
        // reject before persistence. D4 cosmetic URL changes share one identity.
        for value in [
            serde_json::json!({"type":"url","url":"https://mcp.example.test/mcp"}),
            serde_json::json!({"type":"sandbox_stdio","command":"mcp-server","args":["--read-only"]}),
        ] {
            let mut wire =
                serde_json::to_value(server("valid", "https://mcp.example.test/mcp")).unwrap();
            wire["transport"] = value.clone();
            let parsed: McpServerDefinition = serde_json::from_value(wire).unwrap();
            parsed.validate().unwrap();
            assert_eq!(
                serde_json::to_value(parsed).unwrap()["transport"],
                value,
                "D1 exact round trip"
            );
        }
        for url in [
            "file:///tmp/x",
            "https://token@example.test/mcp",
            "https://example.test/mcp#fragment",
            "",
        ] {
            assert!(server("invalid", url).validate().is_err(), "D2");
        }
        let mut stdio = server("stdio", "https://example.test");
        stdio.transport = AgentMcpTransportBinding::sandbox_stdio("", vec![]);
        assert!(stdio.validate().is_err());
        let mut wire = serde_json::to_value(server("x", "https://example.test/mcp")).unwrap();
        wire["transport"]["token"] = serde_json::json!("must-not-be-accepted");
        assert!(
            serde_json::from_value::<McpServerDefinition>(wire).is_err(),
            "D3"
        );
        assert_eq!(
            server("a", "HTTPS://MCP.EXAMPLE.TEST:443/mcp/")
                .target_fingerprint()
                .unwrap(),
            server("b", "https://mcp.example.test/mcp")
                .target_fingerprint()
                .unwrap(),
            "D4"
        );
        assert_ne!(
            server("a", "https://mcp.example.test/mcp")
                .target_fingerprint()
                .unwrap(),
            server("b", "https://mcp.example.test/other")
                .target_fingerprint()
                .unwrap()
        );
    }
    #[test]
    fn cloud_snapshot_preserves_local_definitions_and_exact_defaults() {
        // C1 cloud update valid/newer -> replace cloud snapshot only; C2 replay
        // same revision/content -> no write; C3 stale/conflicting/duplicate ids
        // -> reject without mutation; C4 endpoint changes -> old default stays
        // target-pinned, never silently follows; C5 new source has own revisions.
        let mut state = config();
        state
            .put_local(server("notion", "https://private.example.test/mcp"))
            .unwrap();
        let pin = state
            .server("catalog:notion")
            .unwrap()
            .target_fingerprint()
            .unwrap();
        state.defaults.insert(
            "catalog:notion".into(),
            McpCredentialPreference {
                vault_credential_id: "credential-a".into(),
                target_fingerprint: pin.clone(),
            },
        );
        let mut next = state.catalog.clone();
        next.revision = 2;
        next.servers[0].transport = AgentMcpTransportBinding::http("https://new.example.test/mcp");
        assert!(state.apply_catalog(next.clone()).unwrap(), "C1");
        assert_eq!(
            state.server("local:notion").unwrap().transport,
            AgentMcpTransportBinding::http("https://private.example.test/mcp")
        );
        assert_eq!(
            state.defaults["catalog:notion"].target_fingerprint, pin,
            "C4"
        );
        assert_ne!(
            state
                .server("catalog:notion")
                .unwrap()
                .target_fingerprint()
                .unwrap(),
            pin
        );
        assert!(!state.apply_catalog(next.clone()).unwrap(), "C2");
        let unchanged = state.clone();
        for bad in [
            config().catalog,
            {
                let mut b = next.clone();
                b.servers[0].name = "conflict".into();
                b
            },
            {
                let mut b = next;
                b.revision = 3;
                b.servers.push(b.servers[0].clone());
                b
            },
        ] {
            assert!(state.apply_catalog(bad).is_err(), "C3");
            assert_eq!(state, unchanged);
        }
        state.catalog_url = Some("https://new-source.example.test/catalog.json".into());
        assert!(
            state.apply_catalog(config().catalog).unwrap(),
            "C5 independent source revision"
        );
        assert_eq!(state.catalog_source, state.catalog_url);
    }
    #[test]
    fn workspace_cas_and_default_removal_have_one_owner() {
        // S1 expected revision current -> +1; S2 stale/overflow -> reject;
        // S3 removing a local definition removes its local default only.
        let mut state = config();
        state.advance(0).unwrap();
        validate_mcp_configuration_write(0, &state).unwrap();
        assert!(state.advance(0).is_err(), "S2");
        state.revision = u64::MAX;
        assert!(state.advance(u64::MAX).is_err(), "S2 overflow");
        state.revision = 1;
        state
            .put_local(server("x", "https://example.test/mcp"))
            .unwrap();
        state.defaults.insert(
            "local:x".into(),
            McpCredentialPreference {
                vault_credential_id: "c".into(),
                target_fingerprint: "fp".into(),
            },
        );
        state.remove_local("x");
        assert!(state.defaults.is_empty());
        assert!(state.server("catalog:notion").is_some());
    }
}
