export const BACKEND_URL =
  import.meta.env.VITE_BACKEND_URL ?? "http://localhost:38080";

export const AGENT_ID = import.meta.env.VITE_AGENT_ID ?? "openui-ui";

export function agUiRunUrl(agentId: string): string {
  return `${BACKEND_URL}/v1/ag-ui/agents/${encodeURIComponent(agentId)}/runs`;
}
