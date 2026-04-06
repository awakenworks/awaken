export const adminRoutes = {
  dashboard: "/",
  agents: "/agents",
  agentNew: "/agents/new",
  agent: (id: string) => `/agents/${encodeURIComponent(id)}`,
  skills: "/skills",
  models: "/models",
  providers: "/providers",
  mcpServers: "/mcp-servers",
  assistant: "/assistant",
} as const;
