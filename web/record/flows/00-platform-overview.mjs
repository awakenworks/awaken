// The fast series opener. Every caption points at the exact UI evidence it names;
// seeded, real control-plane objects keep the sweep deterministic and testable.

import { configureSyntheticModel } from "../support/models.mjs";

const AGENT = "platform-overview-agent";
const MODEL = "overview-model";

export const story = {
  promise: "Follow one portable Agent from model supply through governed execution instead of touring unrelated pages.",
  effect: "The same Agent configuration visibly reaches a Managed session with exact Environment and sandbox-policy boundaries.",
  aha: "One portable Agent configuration; models, tools, resources, Environment, and policy are visibly pinned.",
  loyalty: "A coherent end-to-end mental model makes the platform predictable and worth returning to.",
  satisfaction: "Viewers understand where every major capability fits without sitting through a feature inventory.",
  advocacy: "The one-Agent-many-runtimes contrast is concise enough to share as the series trailer.",
};

export async function run({ page, goto, intro, beat, clearCaption, checkpoint, aha, expect, click, wait }) {
  const store = await (await page.request.post("http://127.0.0.1:38080/v1/memory_stores", {
    data: { name: `Overview memory ${Date.now()}` },
  })).json();
  await configureSyntheticModel(page, MODEL);
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT}`, {
    data: {
      id: AGENT,
      name: "Platform overview",
      description: "Shows the complete configurable Agent surface.",
      model: { id: MODEL },
      system: "Translate concise goals into verified work. Use configured Skills for procedural detail.",
      metadata: { owner: "platform" },
      tools: ["read", "write"],
      tool_overrides: [{ target: "mcp__issues__create_issue", alias: "file_issue", description: "Create a verified issue.", defer: true }],
      mcp_servers: [{ type: "url", name: "issues", url: "https://mcp.example.test/issues" }],
      skills: [{ id: "release-review" }],
      max_steps: 12,
      plugins: ["compact", "memory", "state_machine"],
      plugin_config: { compact: {}, memory: {}, state_machine: { machines: [] } },
      context_policy: { kind: "keep_last", keep_last: 24 },
    },
  });
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT}/resources`, {
    data: { agent_id: AGENT, revision: 1, inputs: [{ binding_id: "memory", target: { kind: "memory_store", id: store.id }, mount_path: "/mnt/memory/project", access: "read_write" }] },
  });
  const environment = await (await page.request.post("http://127.0.0.1:38080/v1/environments", {
    data: { name: "Restricted cloud", config: { type: "cloud", networking: { type: "limited" } } },
  })).json();
  await page.request.post("http://127.0.0.1:38080/v1/awaken/sandbox-execution-policies", {
    data: { id: "overview-strict", config: { isolation: "namespace" } },
  });
  await page.request.post(`http://127.0.0.1:38080/v1/awaken/environments/${environment.id}/sandbox-execution-policy`, {
    data: { policy_id: "overview-strict", version: 1 },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT}/publish`);
  expect(published.ok()).toBeTruthy();
  const managedSession = await (await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    data: {
      agent: AGENT,
      environment_id: environment.id,
      title: "One Agent · many realizations",
    },
  })).json();

  await goto("/w/default/overview");
  await intro(
    "See the whole Agent platform in about a minute before choosing a focused workflow.",
    "Awaken controls models, prompts, Skills, MCP, memory, state, protocols, sandboxes, and observable Managed Agents sessions.",
  );
  await beat("Live workspace first: active and running sessions are operational facts, not a static dashboard.", page.locator(".kpis"), 2600);

  await goto("/w/default/models");
  await beat("Models resolve through the visible provider → endpoint → offering chain.", page.locator("tr", { hasText: MODEL }), 2600);

  await goto(`/w/default/agents/${AGENT}`);
  await beat("One guided Agent editor exposes identity, system intent, behavior, tools, integrations, and resources.", page.locator(".editor-rail"), 2800);
  await click(page.getByRole("tab", { name: "Behavior", exact: true }));
  await beat("Behavior makes context, compaction, memory prompts, and the generic State Machine configurable.", page.locator(".editor-content"), 2800);

  await click(page.getByRole("tab", { name: "Tools", exact: true }));
  await beat("Tool Overrides also target runtime-discovered MCP tools by canonical id, with alias and deferred schema.", page.getByLabel("Canonical tool id 1"), 3000);

  await click(page.getByRole("tab", { name: "Integrations", exact: true }));
  await beat("Direct MCP and Skills keep user prompts short: state the goal; the Agent activates procedural detail.", page.locator(".agent-integration-stack"), 3000);

  await click(page.getByRole("tab", { name: "Resources", exact: true }));
  await beat("A bound Memory store is explicit, writable, and mounted into every new Agent session.", page.locator(".editor-content"), 2800);

  await goto("/w/default/environments");
  await beat("Execution stays independent: this official Environment has restricted networking and an exact sandbox policy.", page.locator("tr", { hasText: environment.id }), 3000);

  await goto(`/w/default/sessions/${managedSession.id}`);
  await beat("The story closes in a Managed session pinned to the same Environment.", page.getByText(environment.id, { exact: true }), 3000);

  await checkpoint("the visible sweep is backed by models, MCP, Skill, Memory, State Machine, ACP, and sandbox config", async () => {
    const capsResponse = await page.request.get("http://127.0.0.1:38080/v1/capabilities");
    const configResponse = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT}`);
    const resourceResponse = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT}/resources`);
    expect(capsResponse.ok()).toBeTruthy();
    expect(configResponse.ok()).toBeTruthy();
    expect(resourceResponse.ok()).toBeTruthy();
    const caps = await capsResponse.json();
    const config = await configResponse.json();
    const resources = await resourceResponse.json();
    expect(caps.runtimes.some((runtime) => runtime.id.startsWith("acp:"))).toBeTruthy();
    expect(config.plugins).toEqual(expect.arrayContaining(["memory", "state_machine"]));
    expect(config.mcp_servers[0].name).toBe("issues");
    expect(config.skills[0].id).toBe("release-review");
    expect(config.tool_overrides[0].target).toBe("mcp__issues__create_issue");
    expect(resources.resources[0].resource_id).toBe(store.id);
    const policyBinding = await (await page.request.get(`http://127.0.0.1:38080/v1/awaken/environments/${environment.id}/sandbox-execution-policy`)).json();
    expect(policyBinding).toMatchObject({ policy_id: "overview-strict", version: 1 });
    expect(managedSession.agent.id).toBe(AGENT);
    expect(managedSession.environment_id).toBe(environment.id);
  });

  await clearCaption();
  await aha("One portable Agent configuration with exact Environment, Resource, and policy pins—visibly proven.");
  await wait(700);
  await clearCaption();
}
