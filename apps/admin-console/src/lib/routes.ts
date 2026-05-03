export const adminRoutes = {
  dashboard: "/",
  agents: "/agents",
  agentNew: "/agents/new",
  agent: (id: string) => `/agents/${encodeURIComponent(id)}`,
  agentDashboard: (id: string) =>
    `/agents/${encodeURIComponent(id)}/dashboard`,
  skills: "/skills",
  models: "/models",
  providers: "/providers",
  mcpServers: "/mcp-servers",
  mcpServer: (id: string) => `/mcp-servers/${encodeURIComponent(id)}`,
  skill: (id: string) => `/skills/${encodeURIComponent(id)}`,
  assistant: "/assistant",
  evalReports: "/eval-reports",
  auditLog: "/audit-log",
  auditLogForResource: (resource: string) =>
    `/audit-log?resource=${encodeURIComponent(resource)}`,
} as const;
