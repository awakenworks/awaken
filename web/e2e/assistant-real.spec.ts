import { expect, test, type APIRequestContext } from "@playwright/test";

// Real-model e2e for the Admin Assistant's authoring. The assistant runs on a real model
// (KIMI, submitted through the credential API — no server env), and authors a FULL agent
// config from plain English INCLUDING a data-plane resource binding. This proves "the
// assistant can fill in every part of an agent config": it drafts the agent (persisted
// unpublished) AND binds an existing memory store, and the binding round-trips through
// /v1/config/agents/:id/resources. Its own file (not real-llm.spec) so it self-gates on
// KIMI_KEY independently of the Gemini suite. A developer may also reuse a
// previously validated local vault credential without exposing its secret.
//   Run:  KIMI_EXISTING_CREDENTIAL=1 pnpm exec playwright test assistant-real.spec.ts

const KIMI = process.env.KIMI_KEY ?? "";
const USE_EXISTING_KIMI = process.env.KIMI_EXISTING_CREDENTIAL === "1";
test.skip(!KIMI && !USE_EXISTING_KIMI, "needs KIMI_KEY or KIMI_EXISTING_CREDENTIAL=1");
test.setTimeout(120_000);

// Register KIMI through the config plane, as an operator would (no server env). Wiring a
// model reconciles the Auto-bound assistant onto it.
async function configureKimi(request: APIRequestContext) {
  await request.put("/v1/config/providers/kimi", { data: { id: "kimi", slug: "kimi", display_name: "Kimi", version: 1 } });
  await request.put("/v1/config/endpoints/kimi-ep", { data: { id: "kimi-ep", provider_id: "kimi", dialect: "anthropic_messages", base_url: "https://api.kimi.com/coding/v1/", timeout_secs: 60, display_name: "Kimi", version: 1 } });
  await request.post("/v1/config/offerings", { data: { model_id: "moonshot-v1-8k", provider_id: "kimi", protocol_endpoint_id: "kimi-ep", dialect: "anthropic_messages", upstream_model: null } });
  if (KIMI) {
    await request.post("/v1/config/credentials", { data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "kimi", secret: KIMI } });
  }
}

test("Admin Assistant authors an agent with tools + a memory-store binding (real model)", async ({ page, request }) => {
  await configureKimi(request);
  const ms = (await (await request.post("/v1/memory_stores", { data: { name: "e2e-notes" } })).json()).id as string;
  const agentId = `bound-agent-${Date.now()}`;

  // Drive the assistant in the console (the same chat engine the FAB uses).
  await page.goto("/w/default/assistant");
  const composer = page.getByPlaceholder(/Describe the agent you want|描述你想要的 agent/);
  await expect(composer).toBeVisible({ timeout: 20_000 });
  await composer.fill(
    `Draft an agent id '${agentId}' with the read and write tools, and bind the memory store ` +
      `with resource_id '${ms}' at mount path /mnt/memory/notes with read_write access.`,
  );
  await composer.press("Enter");

  // The draft is a real unpublished agent → a status card with Open-in-editor appears.
  await expect(page.getByRole("button", { name: /Open in editor|在编辑器打开/ }).first()).toBeVisible({ timeout: 90_000 });

  // The full config round-trips: the agent has the tools AND the resource binding.
  const cfg = await (await request.get(`/v1/config/agents/${agentId}`)).json();
  expect(cfg.tools).toEqual(expect.arrayContaining(["read", "write"]));
  const res = await (await request.get(`/v1/config/agents/${agentId}/resources`)).json();
  expect(res.resources?.[0]).toMatchObject({ kind: "memory_store", resource_id: ms });
});

test("Admin Assistant authors an ACP + sandbox environment from plain English (real model)", async ({ request }) => {
  await configureKimi(request);
  // Drive the assistant via its session API (no browser) — it should call
  // admin_draft_environment and persist a real environment through /v1/environments.
  const s = await (await request.post("/v1/sessions", { data: { agent: "__admin_assistant", title: "env-author" } })).json();
  await request.post(`/v1/sessions/${s.id}/events`, {
    data: { events: [{ type: "user.message", content: [{ type: "text", text:
      'Create an execution environment named "e2e-claude-locked" that runs agents with the Claude Code runtime (acp:claude) inside a locked-down sandbox with NO network egress. Use admin_draft_environment.' }] }] },
  });
  // Poll until the assistant settles, then assert the environment persisted with the
  // right runtime + sandbox (the weakest model gets this right because the capability
  // view carries the runtime catalog + sandbox schema).
  const deadline = Date.now() + 90_000;
  let env: { name: string; config: { runtime?: string; sandbox?: { network?: { mode?: string } } } } | undefined;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 2500));
    const envs = (await (await request.get("/v1/environments")).json()).data as typeof env[];
    env = envs.find((e) => e!.name === "e2e-claude-locked");
    if (env) break;
  }
  expect(env, "assistant authored the environment").toBeTruthy();
  expect(env!.config.runtime).toBe("acp:claude");
  expect(env!.config.sandbox?.network?.mode).toBe("none");
});

test("Admin Assistant answers a how-to question as an in-console manual (real model)", async ({ page, request }) => {
  await configureKimi(request);
  await page.goto("/w/default/assistant");
  const composer = page.getByPlaceholder(/Describe the agent you want|描述你想要的 agent/);
  await expect(composer).toBeVisible({ timeout: 20_000 });
  await composer.fill("How do I connect a model to this platform? Keep it short.");
  await composer.press("Enter");
  // A grounded answer (from admin_explain_console) names the real Models/Credentials flow.
  await expect(page.getByText(/Models|Inference credentials|Provider|Offering/i).first()).toBeVisible({ timeout: 90_000 });
});

test("Admin Assistant authors a repeat-safe read-before-write machine (real model)", async ({ request }) => {
  await configureKimi(request);
  const id = `sm-generated-${Date.now()}`;
  const session = await (await request.post("/v1/sessions", {
    data: { agent: "__admin_assistant", title: "sm-author-candidate" },
  })).json();
  await request.post(`/v1/sessions/${session.id}/events`, {
    data: { events: [{ type: "user.message", content: [{ type: "text", text:
      `Draft an unpublished coding agent with id ${id}. Give it the built-in read and write tools. ` +
      "Configure a thread-scoped State Machine that blocks write before execution until the same " +
      "normalized path has been read, but permits repeated writes after that read. Do not publish." }] }] },
  });

  const deadline = Date.now() + 90_000;
  let config: { plugin_config?: { state_machine?: { machines?: Array<{ key_normalizer?: string; transitions?: Array<{ on?: unknown; from?: string | string[]; on_violation?: { action?: string } }> }> } } } | undefined;
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 2500));
    const response = await request.get(`/v1/config/agents/${id}`);
    if (response.ok()) {
      config = await response.json();
      break;
    }
  }

  const machine = config?.plugin_config?.state_machine?.machines?.[0];
  expect(machine, "assistant persisted a state-machine draft").toBeTruthy();
  expect(machine!.key_normalizer).toBe("path");
  const write = machine!.transitions?.find((transition) =>
    typeof transition.on === "string" && transition.on.startsWith("write"));
  expect(write?.on_violation?.action).toBe("deny");
  expect(Array.isArray(write?.from) ? write?.from : [write?.from]).toEqual(
    expect.arrayContaining(["read", "written"]),
  );
});
