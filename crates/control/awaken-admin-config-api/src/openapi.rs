//! OpenAPI 3.1 contract emission for the admin config plane (ADR-0043),
//! mirroring oversight-next's hand-assembled registry: schemas come from the
//! same schemars derives as `export_schemas` (one schema SSOT), and each route
//! in [`crate::admin_router`] is registered here by hand. The
//! `openapi_contract` test probes every documented operation against the real
//! router, so a route added without a registry entry fails the build.
//!
//! Scope is OUR surface only: the Managed Agents wire (sessions/vaults) is the
//! official `@anthropic-ai/sdk` and is deliberately not described here.

use schemars::schema_for;
use serde_json::{Map, Value, json};

/// Every schema component the contract exposes, keyed by component name. This
/// single list feeds both `export_schemas` (JSON Schema bundle → TS types) and
/// [`openapi_document`] (components.schemas), so the two artifacts cannot
/// drift from each other.
#[must_use]
pub fn contract_schemas() -> Map<String, Value> {
    let mut defs: Map<String, Value> = Map::new();
    macro_rules! add {
        ($name:literal, $ty:ty) => {
            defs.insert(
                $name.to_string(),
                serde_json::to_value(schema_for!($ty)).expect("schema serializes"),
            );
        };
    }

    // Model-catalog domain.
    add!("Provider", awaken_model_catalog::Provider);
    add!("ProtocolEndpoint", awaken_model_catalog::ProtocolEndpoint);
    add!("Offering", awaken_model_catalog::Offering);
    add!("ApiDialect", awaken_model_catalog::ApiDialect);
    add!("ProviderCatalog", awaken_model_catalog::ProviderCatalog);

    // Credential domain (secret-free projections only).
    add!(
        "CredentialSource",
        awaken_credential_vault::CredentialSource
    );
    add!(
        "CredentialBinding",
        awaken_credential_vault::CredentialBinding
    );
    add!("CredentialKind", awaken_credential_vault::CredentialKind);
    add!(
        "CredentialStatus",
        awaken_credential_vault::CredentialStatus
    );
    add!(
        "CredentialPoolId",
        awaken_credential_vault::CredentialPoolId
    );
    add!("CredentialPool", awaken_credential_vault::CredentialPool);
    add!(
        "CredentialPoolMember",
        awaken_credential_vault::CredentialPoolMember
    );

    // Authored aggregates (resolver-side domain types).
    add!("InferenceProfile", awaken_config_resolver::InferenceProfile);
    add!("McpServerId", awaken_config_resolver::McpServerId);
    add!("McpServerDef", awaken_config_resolver::McpServerDef);
    add!("AgentMcpConfig", awaken_config_resolver::AgentMcpConfig);
    add!(
        "AgentResourceConfig",
        awaken_config_resolver::AgentResourceConfig
    );

    // Route request bodies (secret-in is write-only by construction).
    add!("EnterCredentialRequest", crate::EnterCredentialRequest);
    add!(
        "ValidateCredentialRequest",
        crate::ValidateCredentialRequest
    );
    add!("ResolveRequest", crate::ResolveRequest);
    add!("ResolveProfileRequest", crate::ResolveProfileRequest);
    add!("ResolveAgentMcpRequest", crate::ResolveAgentMcpRequest);

    // Route responses (secret-free views).
    add!("ResolvedInferenceView", crate::ResolvedInferenceView);
    add!("ResolvedMcpServerView", crate::ResolvedMcpServerView);
    add!("CredentialValidation", crate::CredentialValidation);

    // RFC 9457 problem details (`awaken-api-contract::ApiError`). Hand-written:
    // the foundation crate derives schemars 0.8 while this crate is on
    // schemars 1, so its derive is not reusable here; the wire shape is a
    // standard and stable.
    defs.insert("ApiError".to_string(), api_error_schema());

    defs
}

/// The RFC 9457 Problem Details schema served on every error response.
fn api_error_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "ApiError",
        "description": "RFC 9457 Problem Details with stable application extension members (`code`, `request_id`, `details`, `errors`).",
        "type": "object",
        "properties": {
            "type": { "type": "string", "description": "URI reference identifying the problem type." },
            "title": { "type": "string", "description": "Stable human-readable summary for this problem type." },
            "status": { "type": "integer", "minimum": 100, "maximum": 599 },
            "detail": { "type": "string", "description": "Request-specific human-readable explanation." },
            "instance": { "type": "string" },
            "code": { "type": "string", "description": "Stable application error code; clients may branch on this." },
            "request_id": { "type": "string", "description": "Correlation id echoed from the `x-request-id` header." },
            "details": { "description": "Type-specific structured metadata." },
            "errors": {
                "type": "array",
                "description": "Field/query/header violations for validation-style problems.",
                "items": {
                    "type": "object",
                    "properties": {
                        "field": { "type": "string" },
                        "code": { "type": "string" },
                        "message": { "type": "string" }
                    },
                    "required": ["field", "code"]
                }
            }
        },
        "required": ["type", "title", "status", "code", "request_id"]
    })
}

/// Build the OpenAPI 3.1 document for the admin config plane.
#[must_use]
pub fn openapi_document() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Awaken Admin Config API",
            "version": crate::API_VERSION,
            "summary": "Self-hosted management plane: catalog, credentials, MCP (ADR-0043). Sessions/vaults are the official Anthropic SDK wire and are not described here."
        },
        "tags": [
            { "name": "catalog", "description": "Providers, protocol endpoints, offerings" },
            { "name": "credentials", "description": "Credential sources and pools (secret-in, secret-free-out)" },
            { "name": "inference", "description": "Inference profiles and dry-run resolution" },
            { "name": "mcp", "description": "MCP server definitions and agent bindings" }
        ],
        "paths": paths(),
        "components": { "schemas": Value::Object(contract_schemas()) }
    })
}

fn schema_ref(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

fn array_of(name: &str) -> Value {
    json!({ "type": "array", "items": schema_ref(name) })
}

fn path_param(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "in": "path",
        "required": true,
        "schema": { "type": "string" },
        "description": description
    })
}

/// One operation object. Every operation carries the RFC 9457 `default` error
/// response; `status` is the success status code.
fn op(
    id: &str,
    tag: &str,
    summary: &str,
    params: &[Value],
    request: Option<Value>,
    status: u16,
    response: Value,
) -> Value {
    let mut operation = json!({
        "operationId": id,
        "tags": [tag],
        "summary": summary,
        "responses": {
            status.to_string(): {
                "description": "Success",
                "content": { "application/json": { "schema": response } }
            },
            "default": {
                "description": "Error (RFC 9457 problem details)",
                "content": { "application/problem+json": { "schema": schema_ref("ApiError") } }
            }
        }
    });
    if !params.is_empty() {
        operation["parameters"] = Value::Array(params.to_vec());
    }
    if let Some(schema) = request {
        operation["requestBody"] = json!({
            "required": true,
            "content": { "application/json": { "schema": schema } }
        });
    }
    operation
}

/// The route registry — one entry per route mounted by [`crate::admin_router`].
#[allow(clippy::too_many_lines)]
fn paths() -> Value {
    let id = |desc: &str| vec![path_param("id", desc)];
    json!({
        "/v1/config/providers/{id}": {
            "put": op("put_provider", "catalog", "Author (upsert) a provider; the path id is authoritative",
                &id("Provider id"), Some(schema_ref("Provider")), 200, schema_ref("Provider")),
            "get": op("get_provider", "catalog", "Fetch an authored provider",
                &id("Provider id"), None, 200, schema_ref("Provider"))
        },
        "/v1/config/endpoints/{id}": {
            "put": op("put_endpoint", "catalog", "Author (upsert) a protocol endpoint; the path id is authoritative",
                &id("Protocol endpoint id"), Some(schema_ref("ProtocolEndpoint")), 200, schema_ref("ProtocolEndpoint")),
            "get": op("get_endpoint", "catalog", "Fetch an authored protocol endpoint",
                &id("Protocol endpoint id"), None, 200, schema_ref("ProtocolEndpoint"))
        },
        "/v1/config/offerings": {
            "post": op("post_offering", "catalog", "Author an offering (fail-closed reference integrity to provider/endpoint)",
                &[], Some(schema_ref("Offering")), 200, schema_ref("Offering"))
        },
        "/v1/config/catalog": {
            "get": op("get_catalog", "catalog", "Snapshot the full authored catalog (what the resolver binds against)",
                &[], None, 200, schema_ref("ProviderCatalog"))
        },
        "/v1/config/credentials": {
            "post": op("post_credential", "credentials", "Enter a credential (secret-in; the secret is sealed and never echoed)",
                &[], Some(schema_ref("EnterCredentialRequest")), 201, schema_ref("CredentialSource")),
            "get": op("list_credentials", "credentials", "List a workspace's credential sources (secret-free)",
                &[json!({
                    "name": "workspace_id",
                    "in": "query",
                    "required": true,
                    "schema": { "type": "string" },
                    "description": "Workspace whose sources to list"
                })], None, 200, array_of("CredentialSource"))
        },
        "/v1/config/credentials/{id}": {
            "get": op("get_credential", "credentials", "Fetch one credential source (secret-free)",
                &id("Credential source id"), None, 200, schema_ref("CredentialSource"))
        },
        "/v1/config/credentials/{id}/archive": {
            "post": op("archive_credential", "credentials", "Soft-disable a credential (fails closed at materialization)",
                &id("Credential source id"), None, 200, schema_ref("CredentialSource"))
        },
        "/v1/config/credentials/{id}/validate": {
            "post": op("validate_credential", "credentials", "Live-probe a credential against its provider endpoint (secret-free result)",
                &id("Credential source id"), Some(schema_ref("ValidateCredentialRequest")), 200, schema_ref("CredentialValidation"))
        },
        "/v1/config/credential-pools/{id}": {
            "put": op("put_pool", "credentials", "Author (upsert) a credential pool; the path id is authoritative",
                &id("Credential pool id"), Some(schema_ref("CredentialPool")), 200, schema_ref("CredentialPool")),
            "get": op("get_pool", "credentials", "Fetch a credential pool",
                &id("Credential pool id"), None, 200, schema_ref("CredentialPool"))
        },
        "/v1/config/inference-profiles/{id}": {
            "put": op("put_profile", "inference", "Author (upsert) an inference profile",
                &id("Inference profile id"), Some(schema_ref("InferenceProfile")), 200, schema_ref("InferenceProfile")),
            "get": op("get_profile", "inference", "Fetch an inference profile",
                &id("Inference profile id"), None, 200, schema_ref("InferenceProfile"))
        },
        "/v1/config/inference-profiles/{id}/resolve": {
            "post": op("resolve_profile", "inference", "Dry-run resolve an authored profile (secret-free view)",
                &id("Inference profile id"), Some(schema_ref("ResolveProfileRequest")), 200, schema_ref("ResolvedInferenceView"))
        },
        "/v1/config/inference/resolve": {
            "post": op("resolve_inference", "inference", "Dry-run resolve a model + credential binding against the catalog (secret-free view)",
                &[], Some(schema_ref("ResolveRequest")), 200, schema_ref("ResolvedInferenceView"))
        },
        "/v1/config/mcp-servers": {
            "get": op("list_mcp_servers", "mcp", "List authored MCP server definitions",
                &[], None, 200, array_of("McpServerDef"))
        },
        "/v1/config/mcp-servers/{id}": {
            "put": op("put_mcp_server", "mcp", "Author (upsert) an MCP server definition (credential binding validated fail-closed)",
                &id("MCP server id"), Some(schema_ref("McpServerDef")), 200, schema_ref("McpServerDef")),
            "get": op("get_mcp_server", "mcp", "Fetch an MCP server definition",
                &id("MCP server id"), None, 200, schema_ref("McpServerDef"))
        },
        "/v1/config/agents/{agent_id}/mcp": {
            "put": op("put_agent_mcp", "mcp", "Bind which MCP servers an agent uses at workspace level (fail-closed)",
                &[path_param("agent_id", "Agent id")], Some(schema_ref("AgentMcpConfig")), 200, schema_ref("AgentMcpConfig")),
            "get": op("get_agent_mcp", "mcp", "Fetch an agent's workspace-level MCP binding",
                &[path_param("agent_id", "Agent id")], None, 200, schema_ref("AgentMcpConfig"))
        },
        "/v1/config/agents/{agent_id}/resources": {
            "put": op("put_agent_resource", "mcp", "Bind which resources an agent mounts (ADR-0038); stored whole, path id authoritative",
                &[path_param("agent_id", "Agent id")], Some(schema_ref("AgentResourceConfig")), 200, schema_ref("AgentResourceConfig")),
            "get": op("get_agent_resource", "mcp", "Fetch an agent's resource binding",
                &[path_param("agent_id", "Agent id")], None, 200, schema_ref("AgentResourceConfig"))
        },
        "/v1/config/agents/{agent_id}/mcp/resolve": {
            "post": op("resolve_agent_mcp", "mcp", "Dry-run resolve an agent's MCP binding (secret-free views)",
                &[path_param("agent_id", "Agent id")], Some(schema_ref("ResolveAgentMcpRequest")), 200, array_of("ResolvedMcpServerView"))
        }
    })
}
