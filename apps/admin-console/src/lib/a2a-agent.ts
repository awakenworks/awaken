import type { AgentSpec } from "@/lib/config-api";

export const A2A_SERVER_ID_OPTION = "a2a_server_id";

export function a2aServerIdForAgent(spec: AgentSpec): string | null {
  const option = spec.endpoint?.options?.[A2A_SERVER_ID_OPTION];
  if (typeof option === "string" && option.trim()) return option;
  if (spec.registry?.trim()) return spec.registry;
  return null;
}
