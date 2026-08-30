/** Canonical Managed Agent toolset fixture shared by smoke, E2E, and recordings. */
export function typedAgentTools(names, alwaysAsk = []) {
  const ask = new Set(alwaysAsk);
  return [{
    type: "agent_toolset_20260401",
    default_config: { enabled: false, permission_policy: { type: "always_allow" } },
    configs: names.map((name) => ({
      name,
      enabled: true,
      permission_policy: { type: ask.has(name) ? "always_ask" : "always_allow" },
    })),
  }];
}
