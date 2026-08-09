-- agent mcp bindings: which authored MCP servers an agent uses, one JSON row per agent
CREATE TABLE {prefix}_agent_mcp (
    agent_id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
