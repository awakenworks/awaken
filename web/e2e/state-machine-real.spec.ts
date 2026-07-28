import { expect, test, type APIRequestContext } from "@playwright/test";
import { PRESETS } from "../src/components/agent/state-machine-presets";

// End-to-end proof that the visual editor's state-machine PRESET actually enforces at
// runtime on a real model (KIMI): we build an agent with the exact `read-before-write`
// machine the editor inserts, publish it, and drive the real model to write a file WITHOUT
// reading first — the state machine's `deny` must block the tool call. This closes the loop
// editor-config → published agent → real run → enforcement. It can either enter
// a fresh KIMI_KEY or reuse a previously validated local vault credential.

const KIMI = process.env.KIMI_KEY ?? "";
const KIMI_UPSTREAM_MODEL = process.env.KIMI_MODEL ?? "kimi-for-coding";
const KIMI_MODEL = KIMI_UPSTREAM_MODEL;
const USE_EXISTING_KIMI = process.env.KIMI_EXISTING_CREDENTIAL === "1";
test.skip(!KIMI && !USE_EXISTING_KIMI, "needs KIMI_KEY or KIMI_EXISTING_CREDENTIAL=1");

async function configureKimi(request: APIRequestContext) {
  const credentials = await request.get("/v1/config/credentials?workspace_id=wrkspc_default");
  const existing = (await credentials.json()).find(
    (credential: { provider_id?: string; status: string }) =>
      credential.provider_id === "kimi" && credential.status === "active",
  );
  const response = await request.post("/v1/config/provider-connections", {
    data: {
      workspace_id: "wrkspc_default",
      provider_id: "kimi",
      display_name: "Kimi",
      endpoint_id: "kimi-ep",
      dialect: "anthropic_messages",
      base_url: "https://api.kimi.com/coding/v1/",
      ...(KIMI ? { secret: KIMI } : { credential_source_id: existing?.id }),
    },
  });
  expect(response.status()).toBe(201);
}

test("read-before-write state machine blocks an unread write at runtime (real model)", async ({ request }) => {
  await configureKimi(request);
  const machine = PRESETS.find((p) => p.key === "read-before-write")!.machine;
  const id = `sm-rbw-${Date.now()}`;
  await request.put(`/v1/config/agents/${id}`, {
    data: {
      id,
      name: "SM RBW",
      model: { id: KIMI_MODEL },
      system: "You are a file assistant.",
      tools: ["read", "write"],
      plugins: ["state_machine"],
      // Bypass the permission gate so the state machine is the gate under test.
      plugin_config: {
        permission: { default_behavior: "allow", mode: "bypassPermissions", rules: [] },
        state_machine: { machines: [machine] },
      },
      context_policy: { kind: "keep_all" },
      max_steps: 6,
    },
  });
  await request.post(`/v1/config/agents/${id}/publish`);

  const s = await (await request.post("/v1/sessions", { data: { agent: id, title: "sm-rbw" } })).json();
  await request.post(`/v1/sessions/${s.id}/events`, {
    data: { events: [{ type: "user.message", content: [{ type: "text", text: 'Write the text "hello" to /work/a.txt immediately using the write tool. Do NOT read anything first.' }] }] },
  });

  // Poll until settled, then assert the state machine blocked the write.
  const deadline = Date.now() + 60_000;
  let blocked = false;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 2000));
    const evs = (await (await request.get(`/v1/sessions/${s.id}/events`)).json()).data as Array<{ type: string; content?: Array<{ text?: string }>; stop_reason?: { type?: string } }>;
    blocked = evs.some((e) => e.type === "agent.tool_result" && (e.content ?? []).some((c) => /blocked|Read .* before writing/i.test(c.text ?? "")));
    if (blocked) break;
    const failed = evs.find((event) => event.type === "session.error")
      ?? evs.find((event) => event.type === "session.status_idle" && event.stop_reason?.type === "retries_exhausted");
    if (failed) throw new Error(`real KIMI session failed: ${JSON.stringify(failed)}`);
    if (evs[evs.length - 1]?.type === "session.status_idle") break;
  }
  expect(blocked).toBe(true);
});
