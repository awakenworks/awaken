import { expect, test } from "@playwright/test";

// End-to-end proof (real binary, no mocks) for the ACP sandbox-worker config plane:
//   1. /v1/capabilities advertises the runtime catalog (native + acp:<cli>) and the
//      sandbox schema + presets the console renders.
//   2. An environment persists a full runtime + SandboxSpec config and round-trips.
//   3. A session started against that environment carries the `awaken.runtime`
//      metadata key the host reads to route the run to the ACP backend.
// This closes the loop capability → environment config → session runtime-selection.
// (It stops short of launching the CLI in bwrap — that needs npx + a Linux sandbox
// and is the provisioning slice; here we prove the whole config/selection wire.)

test("capabilities advertise the runtime catalog + sandbox presets", async ({ request }) => {
  const caps = await (await request.get("/v1/capabilities")).json();
  const runtimeIds = (caps.runtimes ?? []).map((r: { id: string }) => r.id);
  expect(runtimeIds).toContain("awaken");
  expect(runtimeIds).toContain("acp:claude");
  // Every acp runtime names its CLI so the host can look it up.
  for (const r of caps.runtimes ?? []) {
    if (r.kind === "acp") expect(typeof r.cli).toBe("string");
  }
  const presetIds = (caps.sandbox?.presets ?? []).map((p: { id: string }) => p.id);
  expect(presetIds).toEqual(["standard", "network-isolated", "locked-down"]);
  expect(caps.sandbox?.config_schema?.properties).toHaveProperty("isolation");
});

test("an environment persists a claude + sandbox config and a session declares its runtime", async ({ request }) => {
  // Author the environment exactly as the console CreateModal does: acp:claude runtime,
  // self-hosted placement, the network-isolated sandbox preset spec.
  const caps = await (await request.get("/v1/capabilities")).json();
  const preset = caps.sandbox.presets.find((p: { id: string }) => p.id === "network-isolated");
  const envRes = await request.post("/v1/environments", {
    data: { name: "claude-sandbox-e2e", config: { type: "self_hosted", runtime: "acp:claude", sandbox: preset.spec } },
  });
  expect(envRes.ok()).toBeTruthy();
  const env = await envRes.json();
  expect(env.config.runtime).toBe("acp:claude");
  expect(env.config.sandbox.network.mode).toBe("allowlist");
  expect(env.config.sandbox.mounts.length).toBeGreaterThan(0);

  // A published agent to bind the session to.
  const id = `sb-e2e-${Date.now()}`;
  await request.put(`/v1/config/agents/${id}`, {
    data: { id, model: { id: "moonshot-v1-8k" }, system: "x", tools: [], plugins: [], context_policy: { kind: "keep_all" }, max_steps: 2 },
  });
  await request.post(`/v1/config/agents/${id}/publish`);

  // Start a session the way sessions.tsx now does: the environment's runtime is
  // stamped onto the session's `awaken.runtime` metadata (the host's ACP selector key).
  const runtime = env.config.runtime as string;
  const s = await (await request.post("/v1/sessions", {
    data: { agent: id, environment_id: env.id, title: "sb", metadata: { "awaken.runtime": runtime } },
  })).json();
  expect(s.metadata["awaken.runtime"]).toBe("acp:claude");
});
