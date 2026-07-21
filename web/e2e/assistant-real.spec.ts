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
const KIMI_UPSTREAM_MODEL = process.env.KIMI_MODEL ?? "kimi-for-coding";
// Keep the catalog identity private to this real-provider suite. Reusing the
// upstream name lets an earlier UI test's synthetic offering hijack resolution.
const KIMI_MODEL = "e2e-real-kimi-for-coding";
const USE_EXISTING_KIMI = process.env.KIMI_EXISTING_CREDENTIAL === "1";
test.skip(!KIMI && !USE_EXISTING_KIMI, "needs KIMI_KEY or KIMI_EXISTING_CREDENTIAL=1");
test.setTimeout(120_000);

// Register KIMI through the config plane, as an operator would (no server env). Wiring a
// model reconciles the Auto-bound assistant onto it.
async function configureKimi(request: APIRequestContext) {
  await request.put("/v1/config/providers/kimi", { data: { id: "kimi", slug: "kimi", display_name: "Kimi", version: 1 } });
  await request.put("/v1/config/endpoints/kimi-ep", { data: { id: "kimi-ep", provider_id: "kimi", dialect: "anthropic_messages", base_url: "https://api.kimi.com/coding/v1/", timeout_secs: 60, display_name: "Kimi", version: 1 } });
  await request.post("/v1/config/offerings", { data: { model_id: KIMI_MODEL, provider_id: "kimi", protocol_endpoint_id: "kimi-ep", dialect: "anthropic_messages", upstream_model: KIMI_UPSTREAM_MODEL } });
  if (KIMI) {
    await request.post("/v1/config/credentials", { data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "kimi", secret: KIMI } });
  }
}

async function runTurn(request: APIRequestContext, sessionId: string, prompt: string): Promise<string> {
  await request.post(`/v1/sessions/${sessionId}/events`, {
    data: { events: [{ type: "user.message", content: [{ type: "text", text: prompt }] }] },
  });
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 1500));
    const events = (await (await request.get(`/v1/sessions/${sessionId}/events`)).json()).data as Array<{
      type: string;
      content?: Array<{ text?: string }>;
      stop_reason?: { type?: string };
    }>;
    const failed = events.find((event) => event.type === "session.error")
      ?? events.find((event) => event.type === "session.status_idle" && event.stop_reason?.type === "retries_exhausted");
    if (failed) throw new Error(`session ${sessionId} failed: ${JSON.stringify(failed)}`);
    const messages = events.filter((event) => event.type === "agent.message");
    if (events.at(-1)?.type === "session.status_idle" && messages.length > 0) {
      return messages.flatMap((message) => (message.content ?? []).map((block) => block.text ?? "")).join("\n");
    }
  }
  throw new Error(`session ${sessionId} did not settle within 90s`);
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
  const open = page.getByRole("button", { name: /Open in editor|在编辑器打开/ }).first();
  const failed = page.getByText(/Run failed|运行失败|session\.error|retries_exhausted/).first();
  await expect(open.or(failed)).toBeVisible({ timeout: 90_000 });
  if (await failed.isVisible()) throw new Error(`Admin Assistant failed before drafting: ${await failed.textContent()}`);

  // The full config round-trips: the agent has the tools AND the resource binding.
  const cfg = await (await request.get(`/v1/config/agents/${agentId}`)).json();
  expect(cfg.tools).toEqual(expect.arrayContaining(["read", "write"]));
  const res = await (await request.get(`/v1/config/agents/${agentId}/resources`)).json();
  expect(res.inputs?.[0]).toMatchObject({ target: { kind: "memory_store", id: ms } });
});

test("KIMI writes and recalls an Agent-bound memory store across fresh sessions", async ({ page, request }) => {
  test.setTimeout(180_000);
  await configureKimi(request);
  const secret = `KIMI-MEMORY-${Date.now()}`;
  const agent = `kimi-memory-${Date.now()}`;
  const store = await (await request.post("/v1/memory_stores", {
    data: { name: `kimi-brain-${Date.now()}` },
  })).json();

  await request.put(`/v1/config/agents/${agent}`, {
    data: {
      id: agent,
      name: agent,
      model: { id: KIMI_MODEL },
      system: "Use the persistent memory file. WRITE facts the user asks you to remember; READ it when asked to recall. Always use the file tools.",
      tools: ["bash", "read", "write", "glob", "grep"],
      plugins: [],
      plugin_config: { permission: { default_behavior: "allow", mode: "bypassPermissions", rules: [] } },
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  });

  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("tab", { name: "Resources", exact: true }).click();
  await page.getByRole("button", { name: /bind a store/ }).click();
  await page.locator("select").nth(1).selectOption({ label: store.name });
  await page.getByRole("button", { name: /Save resources/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Resources saved|资源已保存/ })).toBeVisible();
  const published = await request.post(`/v1/config/agents/${agent}/publish`);
  expect(published.ok()).toBe(true);

  const first = await (await request.post("/v1/sessions", { data: { agent, title: "remember" } })).json();
  await runTurn(request, first.id, `Remember this exact code: ${secret}. Write it to your persistent memory file now.`);
  await request.get(`/v1/files?scope_id=${first.id}`);
  const persisted = await (await request.get(`/v1/memory_stores/${store.id}`)).json();
  expect(persisted.content ?? "").toContain(secret);

  const second = await (await request.post("/v1/sessions", { data: { agent, title: "recall" } })).json();
  const recalled = await runTurn(request, second.id, "Read your persistent memory and answer with only the exact code I asked you to remember.");
  expect(recalled).toContain(secret);
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
    await throwOnSessionFailure(request, s.id);
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
  const answer = page.getByText("⬡ agent").last();
  const failed = page.getByText(/Run failed|运行失败|session\.error|retries_exhausted/).first();
  await expect(answer.or(failed)).toBeVisible({ timeout: 90_000 });
  if (await failed.isVisible()) throw new Error(`Admin Assistant failed before answering: ${await failed.textContent()}`);
  const visibleAnswer = page.locator("main").getByText(/Models|Credentials|provider|offering/i).filter({ visible: true }).last();
  await expect(visibleAnswer).toBeVisible();
  await expect(page.locator("main")).not.toContainText(/Already answered above/i);
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
    await throwOnSessionFailure(request, session.id);
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

test("Publish hands an invalid Draft to KIMI, highlights the repair, then asks only for confirmation", async ({ page, request }) => {
  await configureKimi(request);
  const id = `auto-repair-${Date.now()}`;
  await request.put(`/v1/config/agents/${id}`, {
    data: {
      id,
      system: "Answer release questions from the configured tools.",
      tools: ["tool_that_does_not_exist"],
      plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  });

  await page.goto(`/w/default/agents/${id}`);
  await expect(page.getByLabel("System instructions")).toHaveValue(/release questions/);
  await page.getByRole("button", { name: /Publish/ }).click();

  const modal = page.locator(".modal");
  const failed = page.getByText(/Run failed|运行失败|Needs input|需要处理/).first();
  await expect(modal.or(failed)).toBeVisible({ timeout: 90_000 });
  if (await failed.isVisible()) throw new Error(`KIMI did not repair the Draft: ${await failed.textContent()}`);
  await expect(modal.getByText(/Draft compiled successfully|草稿已通过编译/)).toBeVisible();
  const repaired = await (await request.get(`/v1/config/agents/${id}`)).json();
  expect(repaired.tools).not.toContain("tool_that_does_not_exist");
  await expect(page.getByRole("tab", { name: "Tools" }).locator(".agent-change-dot")).toBeVisible();
});

async function throwOnSessionFailure(request: APIRequestContext, sessionId: string) {
  const response = await request.get(`/v1/sessions/${sessionId}/events`);
  const events = (await response.json()).data as Array<{ type: string; stop_reason?: { type?: string } }>;
  const failed = events.find((event) => event.type === "session.error")
    ?? events.find((event) => event.type === "session.status_idle" && event.stop_reason?.type === "retries_exhausted");
  if (failed) throw new Error(`session ${sessionId} failed: ${JSON.stringify(failed)}`);
}
