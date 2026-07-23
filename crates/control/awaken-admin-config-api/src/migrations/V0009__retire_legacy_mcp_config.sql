-- retire the non-executable legacy MCP catalog and agent binding truth
DROP TABLE IF EXISTS {prefix}_agent_mcp;
DROP TABLE IF EXISTS {prefix}_mcp_server;
