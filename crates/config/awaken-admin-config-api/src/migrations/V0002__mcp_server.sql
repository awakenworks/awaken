-- mcp servers: authored McpServerDef rows (secret-free; credential is a binding by reference)
CREATE TABLE {prefix}_mcp_server (
    id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
