import { expect, test, type APIRequestContext } from "@playwright/test";
import { PRESETS } from "../src/components/agent/state-machine-presets";

// End-to-end proof that the visual editor's state-machine PRESET actually enforces at
// runtime on a real model (KIMI): we build an agent with the exact `read-before-write`
// machine the editor inserts, publish it, and drive the real model to write a file WITHOUT
// reading first — the state machine's `deny` must block the tool call. This closes the loop
// editor-config → published agent → real run → enforcement. Self-gates on KIMI_KEY.

const KIMI = process.env.KIMI_KEY ?? "";
test.skip(!KIMI, "needs KIMI_KEY to drive the state machine on a real model");

async function configureKimi(request: APIRequestContext) {
  await request.put("/v1/config/providers/kimi", { data: { id: "kimi", slug: "kimi", display_name: "Kimi", version: 1 } });
  await request.put("/v1/config/endpoints/kimi-ep", { data: { id: "kimi-ep", provider_id: "kimi", dialect: "anthropic_messages", base_url: "https://api.kimi.com/coding/v1/", timeout_secs: 60, display_name: "Kimi", version: 1 } });
  await request.post("/v1/config/offerings", { data: { model_id: "kimi-k2-0711-preview", provider_id: "kimi", protocol_endpoint_id: "kimi-ep", dialect: "anthropic_messages", upstream_model: null } });
  await request.post("/v1/config/credentials", { data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "kimi", secret: KIMI } });
}

test("read-before-write state machine blocks an unread write at runtime (real model)", async ({ request }) => {
  await configureKimi(request);
  const machine = PRESETS.find((p) => p.key === "read-before-write")!.machine;
  const id = `sm-rbw-${Date.now()}`;
  await request.put(`/v1/config/agents/${id}`, {
    data: {
      id,
      name: "SM RBW",
      model: { id: "kimi-k2-0711-preview" },
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
    const evs = (await (await request.get(`/v1/sessions/${s.id}/events`)).json()).data as Array<{ type: string; content?: Array<{ text?: string }> }>;
    blocked = evs.some((e) => e.type === "agent.tool_result" && (e.content ?? []).some((c) => /blocked|Read .* before writing/i.test(c.text ?? "")));
    if (blocked) break;
    if (evs[evs.length - 1]?.type === "session.status_idle") break;
  }
  expect(blocked).toBe(true);
});
