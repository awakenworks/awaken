import { expect, type APIRequestContext } from "@playwright/test";

export function logicalApiPath(url: string): string {
  return new URL(url).pathname.replace(/^\/v1\/workspaces\/[^/]+/, "/v1");
}

export function isApiRequest(url: string, path: string): boolean {
  return logicalApiPath(url) === path;
}

export async function workspaceId(request: APIRequestContext): Promise<string> {
  const response = await request.get("/v1/config/workspace-context");
  expect(response.ok(), await response.text()).toBe(true);
  const context = await response.json() as { workspace_id: string };
  return context.workspace_id;
}

export async function workspaceApiPath(request: APIRequestContext, path: string): Promise<string> {
  return path.replace(/^\/v1/, `/v1/workspaces/${encodeURIComponent(await workspaceId(request))}`);
}
