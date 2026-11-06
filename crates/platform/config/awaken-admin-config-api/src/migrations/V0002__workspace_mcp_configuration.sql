-- Workspace MCP authoring configuration, independent from execution bindings
CREATE TABLE {prefix}_mcp_configuration (
    workspace_id TEXT PRIMARY KEY,
    revision BIGINT NOT NULL CHECK (revision > 0),
    data {json} NOT NULL
)
