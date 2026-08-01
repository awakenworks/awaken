-- retire the non-executable legacy MCP catalog and agent binding truth
-- migration-allow-edit: adopt the scoped ledger as the sole idempotency mechanism
DROP TABLE {prefix}_agent_mcp;
DROP TABLE {prefix}_mcp_server;
