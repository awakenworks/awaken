import type { CreateSessionRequest } from "./api/types";

export interface SessionMcpServerDraft {
  name: string;
  url: string;
}

export function buildSessionCreateRequest(input: {
  agent: string;
  environmentId: string;
  title: string;
  vaultIds: string[];
  mcpServers: SessionMcpServerDraft[];
}): CreateSessionRequest {
  const mcpServers = input.mcpServers
    .map((server) => ({ name: server.name.trim(), url: server.url.trim() }))
    .filter((server) => server.name.length > 0 && server.url.length > 0)
    .map((server) => ({ type: "url" as const, ...server }));
  return {
    agent: mcpServers.length === 0
      ? input.agent
      : {
          id: input.agent,
          type: "agent_with_overrides",
          mcp_servers: mcpServers,
        },
    environment_id: input.environmentId,
    title: input.title.trim() || undefined,
    vault_ids: input.vaultIds,
  };
}
