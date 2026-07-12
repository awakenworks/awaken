import { expect, test, type APIRequestContext } from "@playwright/test";

// Real-LLM e2e via the CONFIG PLANE (no env model config). The backend runs in plain
// management mode — NO AWAKEN_MODEL_SOURCE / GEMINI_API_KEY on the server. The model
// key enters the platform the way an operator would: submitted through the credential
// API (POST /v1/config/credentials) as a vault secret. The runtime then resolves the
// session's model → offering → workspace credential → a real Gemini executor.
//
// Run against a management backend, with the key available to the TEST (not the server):
//   GOOGLE_API_KEY=… pnpm exec playwright test real-llm.spec.ts
// Not in the default CI suite (no key there).

const KEY = process.env.GOOGLE_API_KEY ?? process.env.GEMINI_API_KEY ?? "";
const REPLY = "⬡ agent";
const REPLY_TIMEOUT = 45_000;

test.skip(!KEY, "needs a Gemini/Google key to submit via the credential API");

// Register Gemini through the config plane exactly as an operator would — no server env.
// Idempotent (PUT provider/endpoint; POST offering/credential tolerate re-runs), so
// multiple tests in this file can each call it against the shared backend.
async function configureGemini(request: APIRequestContext) {
  await request.put("/v1/config/providers/google", { data: { id: "google", slug: "google", display_name: "Google", version: 1 } });
  await request.put("/v1/config/endpoints/gemini-ep", { data: { id: "gemini-ep", provider_id: "google", dialect: "gemini", base_url: null, timeout_secs: 60, display_name: "Gemini", version: 1 } });
  await request.post("/v1/config/offerings", { data: { model_id: "gemini-2.5-flash", provider_id: "google", protocol_endpoint_id: "gemini-ep", dialect: "gemini", upstream_model: null } });
  await request.post("/v1/config/credentials", { data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "google", secret: KEY } });
}

// Send one user turn and poll the session's event log until the assistant turn settles
// (status idle with at least one agent.message). Returns the concatenated assistant text.
async function runTurn(request: APIRequestContext, sessionId: string, text: string): Promise<string> {
  await request.post(`/v1/sessions/${sessionId}/events`, {
    data: { events: [{ type: "user.message", content: [{ type: "text", text }] }] },
  });
  const deadline = Date.now() + REPLY_TIMEOUT;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 1500));
    const evs = (await (await request.get(`/v1/sessions/${sessionId}/events`)).json()).data as Array<{ type: string; content?: Array<{ text?: string }> }>;
    // Surface an upstream turn failure (e.g. an exhausted model quota) as the error it
    // is, rather than a mute timeout — the message tells the operator to check quota/key.
    if (evs.some((e) => e.type === "session.error")) {
      throw new Error(`session ${sessionId} errored (check model quota/credential): ${JSON.stringify(evs.filter((e) => e.type === "session.error"))}`);
    }
    const last = evs[evs.length - 1]?.type;
    const messages = evs.filter((e) => e.type === "agent.message");
    if (last === "session.status_idle" && messages.length > 0) {
      return messages.flatMap((m) => (m.content ?? []).map((c) => c.text ?? "")).join("\n");
    }
  }
  throw new Error(`session ${sessionId} did not settle within ${REPLY_TIMEOUT}ms`);
}

test("Sandbox answers for real via config-plane credential (no env)", async ({ page, request }) => {
  const id = `real-agent-${Date.now()}`;

  // Operator-style setup, all through the config-plane API — no server env.
  await configureGemini(request);

  // Author + publish an agent bound to that model, via the console.
  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.locator("select").first().selectOption("gemini-2.5-flash");
  await page.locator("textarea").first().fill("You are a terse assistant. Answer in one short sentence.");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));
  await page.getByRole("button", { name: /Publish/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Published|已发布/ })).toBeVisible();

  // Sandbox: the runtime resolves the model to a REAL Gemini executor from the
  // configured credential (no env), and a real reply lands on screen.
  await page.getByRole("button", { name: "Try it", exact: true }).click();
  await page.getByRole("button", { name: /Start session/ }).click();
  const ask = page.getByPlaceholder("Ask the agent…");
  await expect(ask).toBeVisible();
  await ask.fill("Say hello.");
  await ask.press("Enter");
  await expect(page.getByText(REPLY).first()).toBeVisible({ timeout: REPLY_TIMEOUT });
});

// The build→bind→use loop, proven with a REAL model: a memory store bound to a
// published agent (via the console Resources tab) is actually MOUNTED in every session
// the agent runs, is WRITABLE, and PERSISTS across sessions. This is the payoff of
// wiring the binding store into `prepare_session` — the config plane injects the prompt,
// the runtime injects the mount, and the same store id survives write-back.
//
//   Session 1 → the agent writes a secret to its memory file.
//   Harvest    → GET /v1/files?scope_id persists the write into the store blob.
//   Session 2  → a FRESH session (no chat history) reads the file back and reports it.
//
// A brand-new secret each run means a stale blob can never false-pass.
test("Agent reads/writes its bound memory store across sessions (real model)", async ({ page, request }) => {
  test.setTimeout(150_000); // UI bind + two real Gemini turns (write, then read-back)
  const secret = `BANANA-${Date.now()}`;
  const agent = `mem-agent-${Date.now()}`;
  await configureGemini(request);

  // A memory store to bind. Author the agent with file tools + a bypass permission
  // policy (a note-keeper should read/write its own memory autonomously), publish it.
  const store = await (await request.post("/v1/memory_stores", { data: { name: `brain-${Date.now()}` } })).json();
  await request.put(`/v1/config/agents/${agent}`, {
    data: {
      id: agent,
      name: agent,
      model: { id: "gemini-2.5-flash" },
      system:
        "You are a note-keeping agent with a persistent memory file. To remember something, WRITE it to the memory file. To recall, READ the memory file. Always use your tools.",
      tools: ["bash", "read", "write", "glob", "grep"],
      plugins: [],
      plugin_config: { permission: { default_behavior: "allow", mode: "bypassPermissions", rules: [] } },
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  });

  // Bind the store to the agent through the SAME console tab a user would use.
  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("button", { name: "Resources", exact: true }).click();
  await page.getByRole("button", { name: /bind a store/ }).click();
  await page.locator("select").nth(1).selectOption({ label: store.name }); // 0=kind, 1=store
  await page.getByRole("button", { name: /Save resources/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Resources saved|资源已保存/ })).toBeVisible();
  await request.post(`/v1/config/agents/${agent}/publish`);

  // Session 1: the agent writes the secret into its bound memory.
  const s1 = await (await request.post("/v1/sessions", { data: { agent, title: "write" } })).json();
  const wrote = await runTurn(request, s1.id, `Save this to your memory so you never forget it: the secret code is ${secret}. Write it to your memory file now.`);
  expect(wrote.length).toBeGreaterThan(0);

  // Harvest: persist the session's memory write-back into the durable store blob.
  await request.get(`/v1/files?scope_id=${s1.id}`);
  const stored = await (await request.get(`/v1/memory_stores/${store.id}`)).json();
  expect(stored.content ?? "").toContain(secret);

  // Session 2: a fresh session (no shared history) reads the persisted memory back.
  const s2 = await (await request.post("/v1/sessions", { data: { agent, title: "read" } })).json();
  const recalled = await runTurn(request, s2.id, "Read your memory file and tell me the secret code. Answer with only the code.");
  expect(recalled).toContain(secret);
});

// A bound read-only FILE is mounted and read by a real model — the file kind of the same
// loop, and a cleaner single-session proof: the blob is seeded at upload (no write-back
// dance), so one turn reads it back. Proves file bindings realize as sandbox mounts the
// agent's tools can read, described in the compiled system prompt at their `.mnt/` path.
test("Agent reads a bound read-only file (real model)", async ({ request }) => {
  test.setTimeout(90_000);
  const secret = `MANGO-${Date.now()}`;
  const agent = `file-agent-${Date.now()}`;
  await configureGemini(request);

  // Upload a file carrying the secret, author a read-capable agent, bind + publish.
  const file = await (
    await request.post("/v1/files", {
      multipart: { file: { name: "config.txt", mimeType: "text/plain", buffer: Buffer.from(`the launch code is ${secret}`) }, purpose: "agent" },
    })
  ).json();
  await request.put(`/v1/config/agents/${agent}`, {
    data: {
      id: agent,
      name: agent,
      model: { id: "gemini-2.5-flash" },
      system: "You can read files with your tools. When asked about a file, READ it and answer from its contents.",
      tools: ["bash", "read", "glob", "grep"],
      plugins: [],
      plugin_config: { permission: { default_behavior: "allow", mode: "bypassPermissions", rules: [] } },
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  });
  await request.put(`/v1/config/agents/${agent}/resources`, {
    data: { agent_id: agent, version: 1, resources: [{ kind: "file", resource_id: file.id, mount_path: "/data/config.txt", access: "read_only" }] },
  });
  await request.post(`/v1/config/agents/${agent}/publish`);

  // One session: the agent reads the mounted file and reports the secret.
  const s = await (await request.post("/v1/sessions", { data: { agent, title: "read-file" } })).json();
  const answer = await runTurn(request, s.id, "Read the config file that is mounted for you and tell me the launch code. Answer with only the code.");
  expect(answer).toContain(secret);
});
