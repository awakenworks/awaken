export const qk = {
  navHealth: () => ["nav-health"] as const,
  config: {
    all: ["config"] as const,
    listRoot: (namespace: string) => ["config", "list", namespace] as const,
    list: (namespace: string, offset = 0, limit = 100) =>
      ["config", "list", namespace, offset, limit] as const,
    listWithAux: (namespace: string, auxiliaryKey: readonly unknown[], offset = 0, limit = 100) =>
      ["config", "list", namespace, offset, limit, "aux", ...auxiliaryKey] as const,
    get: (namespace: string, id: string) => ["config", "get", namespace, id] as const,
    meta: (namespace: string, id: string) => ["config", "meta", namespace, id] as const,
    listMeta: (namespace: string) => ["config", "meta", namespace] as const,
  },
  capabilities: () => ["capabilities"] as const,
  audit: {
    log: (query: unknown) => ["audit", "log", query] as const,
  },
  dashboardRoot: () => ["dashboard"] as const,
  dashboard: (range: string) => ["dashboard", range] as const,
  mcp: {
    status: (id: string) => ["mcp", "status", id] as const,
  },
  agent: {
    runtimeStats: (id: string, window: string) =>
      ["agent", "runtime-stats", id, window || "default"] as const,
    runtimeStatsList: () => ["agent", "runtime-stats"] as const,
  },
  system: {
    info: () => ["system", "info"] as const,
  },
};
