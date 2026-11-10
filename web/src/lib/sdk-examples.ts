// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: One Managed Agents SDK snippet template shared by the Quickstart integrate step and the
// @formal reason: protocol directory, so both show the same example. Static content; it calls nothing.
// @formal reason: Classification only, not formal evidence.
// The Managed Agents SDK example. It lives outside the protocol directory so
// the Quickstart integrate step can show the same snippet without importing
// that whole surface; the directory re-exports it, keeping one template.

import managedSource from "@awaken/application-example/managed.mjs?raw";

export interface ManagedSdkContext {
  agentId?: string | null;
  environmentId?: string | null;
  workspaceId?: string | null;
}

// The runnable example owns its behavior; Console personalizes only coordinates.
export function managedSdkExample(context: ManagedSdkContext = {}): string {
  let source = managedSource;
  for (const [value, expression] of [
    [context.agentId, 'process.env.AWAKEN_AGENT_ID'],
    [context.environmentId, 'process.env.AWAKEN_ENVIRONMENT_ID'],
    [context.workspaceId, 'process.env.AWAKEN_WORKSPACE_ID'],
  ] as const) {
    if (value?.trim()) source = source.replaceAll(expression, JSON.stringify(value));
  }
  return source;
}
