import type { APIRequestContext } from "@playwright/test";
import { createServer, type Server } from "node:http";

export class SyntheticModelDirectory {
  private readonly models = new Set<string>();
  private server?: Server;
  private baseUrl = "";

  async start() {
    this.server = createServer((_request, response) => {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({
        data: Array.from(this.models).map((id) => ({ id })),
        has_more: false,
      }));
    });
    await new Promise<void>((resolve, reject) => {
      this.server!.once("error", reject);
      this.server!.listen(0, "127.0.0.1", resolve);
    });
    const address = this.server.address();
    if (!address || typeof address === "string") {
      throw new Error("synthetic model directory did not bind");
    }
    this.baseUrl = `http://127.0.0.1:${address.port}`;
  }

  async stop() {
    if (!this.server) return;
    await new Promise<void>((resolve, reject) =>
      this.server!.close((error) => error ? reject(error) : resolve()));
  }

  async configure(request: APIRequestContext, model: string) {
    this.models.add(model);
    const workspaceResponse = await request.get("/v1/config/workspace-context");
    if (!workspaceResponse.ok()) {
      throw new Error(await workspaceResponse.text());
    }
    const workspace = await workspaceResponse.json();
    const credentialsResponse = await request.get(
      `/v1/config/credentials?workspace_id=${encodeURIComponent(workspace.workspace_id)}`,
    );
    if (!credentialsResponse.ok()) {
      throw new Error(await credentialsResponse.text());
    }
    const credentials = await credentialsResponse.json() as Array<{
      id: string;
      provider_id?: string;
      status?: string;
    }>;
    const existing = credentials.find((source) =>
      source.provider_id === "anthropic" && source.status === "active");
    const response = await request.post("/v1/config/provider-connections", {
      data: {
        idempotency_key: `e2e-synthetic-${model}`,
        workspace_id: workspace.workspace_id,
        provider_id: "anthropic",
        display_name: "E2E fixture",
        dialect: "anthropic_messages",
        base_url: `${this.baseUrl}/v1/`,
        timeout_secs: 60,
        ...(existing
          ? { credential_source_id: existing.id }
          : { secret: "sk-synthetic-e2e" }), // awaken-allow: secret (synthetic e2e fixture)
      },
    });
    if (!response.ok()) {
      throw new Error(await response.text());
    }
  }
}
