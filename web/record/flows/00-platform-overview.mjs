// The fast series opener. Every caption points at the exact UI evidence it names;
// seeded, real control-plane objects keep the sweep deterministic and testable.

const AGENT = "platform-overview-agent";
const MODEL = "overview-model";

export async function run({ page, goto, intro, beat, clearCaption, checkpoint, aha, expect, click, wait }) {
  const store = await (await page.request.post("http://127.0.0.1:38080/v1/memory_stores", {
    data: { name: `Overview memory ${Date.now()}` },
  })).json();
  await page.request.put("http://127.0.0.1:38080/v1/config/providers/overview", {
    data: { id: "overview", slug: "overview", display_name: "Overview provider", version: 1 },
  });
  await page.request.put("http://127.0.0.1:38080/v1/config/endpoints/overview-anthropic", {
    data: { id: "overview-anthropic", provider_id: "overview", dialect: "anthropic_messages", base_url: "https://api.example.test/v1", timeout_secs: 60, display_name: "Overview endpoint", version: 1 },
  });
  await page.request.post("http://127.0.0.1:38080/v1/config/offerings", {
    data: { model_id: MODEL, provider_id: "overview", protocol_endpoint_id: "overview-anthropic", dialect: "anthropic_messages", upstream_model: "demo-upstream" },
  });
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
    data: { agent_id: AGENT, version: 1, resources: [{ kind: "memory_store", resource_id: store.id, mount_path: "/mnt/memory/project", access: "read_write" }] },
  });
  const environment = await (await page.request.post("http://127.0.0.1:38080/v1/environments", {
    data: { name: "Claude Code · locked", config: { type: "cloud", runtime: "acp:claude", sandbox: { isolation: "namespace", network: { mode: "none" }, limits: {} } } },
  })).json();
  await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    data: { agent: "default", title: "Overview proof session" },
  });

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
  await beat("Execution stays independent: this environment combines Claude Code over ACP with a no-egress sandbox.", page.locator("tr", { hasText: environment.id }), 3000);

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
    expect(environment.config.runtime).toBe("acp:claude");
    expect(environment.config.sandbox.network.mode).toBe("none");
  });

  await clearCaption();
  await aha("One portable Agent configuration; many model, tool, protocol, and sandbox realizations—each visibly proven.");
  await wait(700);
  await clearCaption();
}
