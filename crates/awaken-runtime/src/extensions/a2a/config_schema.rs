use crate::backend::ExecutionBackendConfigSchema;
use serde_json::json;

pub(super) fn a2a_backend_config_schema() -> ExecutionBackendConfigSchema {
    ExecutionBackendConfigSchema {
        version: 1,
        schema: json!({
            "type": "object",
            "title": "A2A backend config",
            "description": "HTTP+JSON A2A remote agent endpoint configuration.",
            "additionalProperties": false,
            "required": ["base_url"],
            "properties": {
                "base_url": {
                    "type": "string",
                    "title": "Base URL",
                    "minLength": 1
                },
                "auth": {
                    "title": "Authentication",
                    "anyOf": [
                        {
                            "title": "None",
                            "type": "null"
                        },
                        {
                            "title": "Bearer token",
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["type", "token"],
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "const": "bearer",
                                    "default": "bearer"
                                },
                                "token": {
                                    "type": "string",
                                    "title": "Token",
                                    "minLength": 1
                                }
                            }
                        }
                    ],
                    "default": null
                },
                "target": {
                    "type": ["string", "null"],
                    "title": "Target agent",
                    "description": "Backend-specific target resource, typically the remote agent id."
                },
                "timeout_ms": {
                    "type": "integer",
                    "title": "Timeout (ms)",
                    "minimum": 1,
                    "default": 300000
                },
                "options": {
                    "type": "object",
                    "title": "Options",
                    "additionalProperties": true,
                    "default": {}
                }
            }
        }),
        display_name: Some("A2A".to_string()),
        description: Some("Run the agent through a remote A2A HTTP+JSON endpoint.".to_string()),
        default_config: json!({
            "base_url": "",
            "auth": null,
            "target": null,
            "timeout_ms": 300000,
            "options": {},
        }),
        ui_schema: Some(json!({
            "auth": {
                "token": {
                    "ui:widget": "password"
                }
            }
        })),
    }
}
