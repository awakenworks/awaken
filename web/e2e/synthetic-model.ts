import type { APIRequestContext } from "@playwright/test";
import { startFakeAnthropic } from "../../e2e/fixtures/fake_anthropic_fixture.mjs";

const SYNTHETIC_API_KEY = "sk-synthetic-e2e"; // awaken-allow: secret (synthetic e2e fixture)

export class SyntheticModelDirectory {
  private readonly models: string[] = [];
  private upstream?: Awaited<ReturnType<typeof startFakeAnthropic>>;
  private baseUrl = "";

  async start() {
    // One canonical Anthropic wire fixture owns discovery and inference. The
    // earlier directory-only HTTP fake returned the model list for authoring
    // but answered a real Quickstart /messages request with that list shape,
    // leaving its durable Session active and starving later browser scenarios.
    this.upstream = await startFakeAnthropic(SYNTHETIC_API_KEY, { models: this.models });
    this.baseUrl = this.upstream.url;
  }

  async stop() {
    if (!this.upstream) return;
    await this.upstream.close();
    this.upstream = undefined;
  }

  async configure(
    request: APIRequestContext,
    model: string,
    configBase = "/v1/config",
  ): Promise<string> {
    if (!this.models.includes(model)) this.models.push(model);
    const workspaceResponse = await request.get(`${configBase}/workspace-context`);
    if (!workspaceResponse.ok()) {
      throw new Error(await workspaceResponse.text());
    }
    const executionWorkspace = workspaceResponse.headers()["anthropic-workspace-id"];
    if (!executionWorkspace) {
      throw new Error("workspace context omitted anthropic-workspace-id");
    }
    const credentialsResponse = await request.get(
      `${configBase}/credentials?workspace_id=${encodeURIComponent(executionWorkspace)}`,
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
    const response = await request.post(`${configBase}/provider-connections`, {
      data: {
        idempotency_key: `e2e-synthetic-${model}`,
        workspace_id: executionWorkspace,
        provider_id: "anthropic",
        display_name: "E2E fixture",
        dialect: "anthropic_messages",
        base_url: `${this.baseUrl}/v1/`,
        timeout_secs: 60,
        ...(existing
          ? { credential_source_id: existing.id }
          : { secret: SYNTHETIC_API_KEY }),
      },
    });
    if (!response.ok()) {
      throw new Error(await response.text());
    }
    const deadline = Date.now() + 20_000;
    let lastProjection = "no executable-model projection";
    while (Date.now() < deadline) {
      const projection = await request.get(
        `${configBase}/executable-models?workspace_id=${encodeURIComponent(executionWorkspace)}`,
      );
      lastProjection = await projection.text();
      if (projection.ok()) {
        const models = JSON.parse(lastProjection) as Array<{
          model_id?: string;
          readiness?: string;
        }>;
        if (models.some((candidate) =>
          candidate.model_id === model && candidate.readiness === "ready")) {
          return executionWorkspace;
        }
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`synthetic model did not become executable: ${lastProjection}`);
  }
}
