// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: One Managed Agents SDK snippet template shared by the Quickstart integrate step and the
// @formal reason: protocol directory, so both show the same example. Static content; it calls nothing.
// @formal reason: Classification only, not formal evidence.
// The Managed Agents SDK example. It lives outside the protocol directory so
// the Quickstart integrate step can show the same snippet without importing
// that whole surface; the directory re-exports it, keeping one template.

export interface ManagedSdkContext {
  agentId?: string | null;
  environmentId?: string | null;
  workspaceId?: string | null;
}

function sdkCoordinate(value: string | null | undefined, environmentVariable: string): string {
  return value?.trim() ? JSON.stringify(value) : `process.env.${environmentVariable}`;
}

/** Derive a contextual example from one SDK template. Query coordinates only
 * replace two copy-time values and never become configuration authority. */
export function managedSdkExample(context: ManagedSdkContext = {}): string {
  const agent = sdkCoordinate(context.agentId, "AWAKEN_AGENT_ID");
  const environment = sdkCoordinate(context.environmentId, "AWAKEN_ENVIRONMENT_ID");
  const workspace = encodeURIComponent(context.workspaceId?.trim() || "default");
  return `// npm install @anthropic-ai/sdk
import Anthropic from "@anthropic-ai/sdk";

const betas = ["managed-agents-2026-04-01"];

const awaken = new Anthropic({
  apiKey: process.env.AWAKEN_API_KEY,
  baseURL: process.env.AWAKEN_BASE_URL,
});

const session = await awaken.beta.sessions.create({
  agent: ${agent},
  environment_id: ${environment},
  betas,
});

await awaken.beta.sessions.events.send(session.id, {
  events: [{
    type: "user.message",
    content: [{ type: "text", text: "Review this support escalation." }],
  }],
  betas,
});

console.log(\`Open in Console: /w/${workspace}/sessions/\${session.id}\`);`;
}
