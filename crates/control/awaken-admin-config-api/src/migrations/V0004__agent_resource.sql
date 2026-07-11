-- agent resource bindings: which resources an agent is bound to (ADR-0038), one JSON row per agent
CREATE TABLE {prefix}_agent_resource (
    agent_id TEXT PRIMARY KEY,
    data {json} NOT NULL,
    created_at {timestamptz} NOT NULL DEFAULT {now}
)
