#!/usr/bin/env node
// Console ⇄ backend integration smoke: replays the exact request sequence the
// surfaces issue (same paths, same bodies) against a management-mode host.
//   AWAKEN_HTTP_URL=http://127.0.0.1:38091 node web/scripts/smoke.mjs

import { createServer } from "node:http";

const BASE = process.env.AWAKEN_HTTP_URL ?? "http://127.0.0.1:38080";
let failures = 0;

async function step(name, method, path, body, check) {
  const res = await fetch(BASE + path, {
    method,
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  let payload = null;
  try {
    payload = await res.json();
  } catch {
    /* empty body */
  }
  const ok = check ? check(res.status, payload) : res.ok;
  console.log(`${ok ? "✓" : "✗"} ${name} → ${res.status}`);
  if (!ok) {
    failures += 1;
    console.log("  ", JSON.stringify(payload)?.slice(0, 300));
  }
  return payload;
}

// Multipart upload (Files + Skills APIs take bytes, not JSON) — Node's global
// FormData/Blob/fetch. Returns the parsed JSON payload.
async function uploadStep(name, path, filename, mime, text, fields, check) {
  const form = new FormData();
  form.append("file", new Blob([text], { type: mime }), filename);
  for (const [k, v] of Object.entries(fields ?? {})) form.append(k, v);
  const res = await fetch(BASE + path, { method: "POST", body: form });
  let payload = null;
  try {
    payload = await res.json();
  } catch {
    /* empty body */
  }
  const ok = check ? check(res.status, payload) : res.ok;
  console.log(`${ok ? "✓" : "✗"} ${name} → ${res.status}`);
  if (!ok) {
    failures += 1;
    console.log("  ", JSON.stringify(payload)?.slice(0, 300));
  }
  return payload;
}

// ---- Workspace · Provider Connections (surfaces/models.tsx) ----
// The real command verifies provider discovery before committing any catalog
// facts. This local directory is transport-only test evidence; the backend still
// runs the production connection/discovery/commit path.
const directory = createServer((request, response) => {
  if (request.url?.startsWith("/v1/models")) {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({
      data: [{ id: "claude-sonnet-4-5" }],
      has_more: false,
    }));
    return;
  }
  response.writeHead(404);
  response.end();
});
await new Promise((resolve, reject) => {
  directory.once("error", reject);
  directory.listen(0, "127.0.0.1", resolve);
});
directory.unref();
const directoryAddress = directory.address();
if (typeof directoryAddress !== "object" || directoryAddress === null) {
  throw new Error("model directory did not expose a TCP address");
}
const existingCredentials = await step(
  "read reusable credentials",
  "GET",
  "/v1/config/credentials?workspace_id=wrkspc_default",
  undefined,
  (s, p) => s === 200 && Array.isArray(p),
);
const reusableCredential = existingCredentials.find(
  (source) =>
    source.status === "active" &&
    source.provider_id === "anthropic" &&
    source.env_key !== "CLAUDE_CODE_OAUTH_TOKEN",
);
const connection = await step("verify and save provider connection", "POST", "/v1/config/provider-connections", {
  workspace_id: "wrkspc_default",
  provider_id: "anthropic",
  display_name: "Anthropic",
  endpoint_id: "anthropic-messages",
  dialect: "anthropic_messages",
  base_url: `http://127.0.0.1:${directoryAddress.port}/v1`,
  timeout_secs: 60,
  ...(reusableCredential
    ? { credential_source_id: reusableCredential.id }
    : { secret: "sk-test-not-a-real-key" }), // awaken-allow: secret (synthetic smoke fixture)
}, (s, p) => s === 201 && p.sync.discovered === 1 && p.credential.status === "active");
const cred = connection.credential;
await step("catalog snapshot", "GET", "/v1/config/catalog", undefined, (s, p) => s === 200 && p.offerings.length >= 1);

// ---- Workspace · Credentials inventory (surfaces/credentials.tsx) ----
await step("list credentials", "GET", "/v1/config/credentials?workspace_id=wrkspc_default", undefined, (s, p) => s === 200 && Array.isArray(p) && p.length >= 1);
await step("resolve dry-run (exact)", "POST", "/v1/config/inference/resolve", {
  workspace_id: "wrkspc_default",
  target: { model_id: "claude-sonnet-4-5" },
  binding: { type: "exact", credential_source_id: cred.id },
}, (s, p) => s === 200 && p.credential_present === true);

// ---- Settings: project authoring (surfaces/settings.tsx) ----
// Project was removed from the tenancy model (ADR-0051 `remove Project` +
// ADR-0048): there is no `/v1/config/projects` resource and no `/projects/{id}`
// ingress; the tenant is the workspace (addressed by key or `/v1/workspaces/{ws}/…`).

// ---- Workspace · Agents authoring via config plane (surfaces/agent-editor.tsx) ----
// The console authors the rich AgentConfig against our own management API, then
// publishes (compile + install). Publish/compile is inference-agnostic.
// The object model IS the managed /v1/agents object (name/model/system/tools/…)
// plus extensions (plugins/plugin_config/context_policy/max_steps).
const cfgAgent = {
  id: "smoke-agent",
  name: "Smoke Agent",
  model: { id: "claude" },
  system: "You are a smoke-test agent.",
  tools: ["read"],
  mcp_servers: [{
    type: "url",
    name: "docs",
    url: "https://mcp.example.com/docs",
  }],
  skills: [],
  max_steps: 8,
  plugins: [],
  plugin_config: {},
  context_policy: { kind: "keep_all" },
  // Tool presentation (ADR-0053): rename a static tool + alias/defer an MCP tool.
  tool_overrides: [
    { target: "read", alias: "open_file", description: "Read a file." },
    { target: "mcp__docs__search", alias: "docs", defer: true },
  ],
};
await step("author config agent", "PUT", "/v1/config/agents/smoke-agent", cfgAgent, (s, p) => s === 200 && p.id === "smoke-agent");
await step("validate config agent", "POST", "/v1/config/agents/smoke-agent/validate", cfgAgent, (s, p) => s === 200 && p.valid === true);
// The stored object round-trips in the managed shape: model {id}, system, published flag.
await step("get config agent (managed shape)", "GET", "/v1/config/agents/smoke-agent", undefined, (s, p) => s === 200 && p.type === "agent" && p.model?.id === "claude" && p.system === "You are a smoke-test agent." && p.published === false);
// Tool presentation overrides round-trip in the managed shape (ADR-0053).
await step("tool_overrides round-trip", "GET", "/v1/config/agents/smoke-agent", undefined, (s, p) =>
  s === 200 &&
  p.tool_overrides?.length === 2 &&
  p.tool_overrides.some((o) => o.target === "read" && o.alias === "open_file") &&
  p.tool_overrides.some((o) => o.target === "mcp__docs__search" && o.defer === true));
await step("list config agents (draft)", "GET", "/v1/config/agents", undefined, (s, p) => s === 200 && p.data.some((a) => a.id === "smoke-agent" && a.published === false));
await step("publish config agent", "POST", "/v1/config/agents/smoke-agent/publish", undefined, (s, p) => s === 200 && p.installed === true);
await step("list config agents (published)", "GET", "/v1/config/agents", undefined, (s, p) => s === 200 && p.data.some((a) => a.id === "smoke-agent" && a.published === true));

// ---- Capability snapshot (surfaces/agent-editor data-driven pickers) ----
await step("capabilities (flat)", "GET", "/v1/capabilities", undefined, (s, p) =>
  s === 200 && Array.isArray(p.tools) && p.tools.length > 0 &&
  p.plugins.some((pl) => pl.id === "state_machine" && pl.config_schema && typeof pl.config_schema === "object"));
// Uniform addressing: same snapshot under the workspace path prefix (ADR-0048).
await step("capabilities (workspace-path)", "GET", "/v1/workspaces/default/capabilities", undefined, (s, p) => s === 200 && p.plugins.length > 0);
// The permission policy is advertised as a policy (not a plugin), with its schema —
// the PermissionEditor consumes this to author the `permission` section.
await step("capabilities exposes the permission policy", "GET", "/v1/capabilities", undefined, (s, p) =>
  s === 200 && Array.isArray(p.policies) &&
  p.policies.some((pl) => pl.id === "permission" && pl.config_schema && typeof pl.config_schema === "object"));

// ---- Permission policy authoring round-trips (surfaces/agent-editor PermissionEditor) ----
const permAgent = {
  id: "perm-agent",
  name: "Perm Agent",
  model: { id: "claude" },
  system: "You gate your tools.",
  tools: [],
  mcp_servers: [],
  skills: [],
  max_steps: 8,
  plugins: [],
  plugin_config: { permission: { default_behavior: "deny", rules: [{ pattern: "Bash(*rm*)", behavior: "deny" }] } },
  context_policy: { kind: "keep_all" },
};
await step("author agent with permission policy", "PUT", "/v1/config/agents/perm-agent", permAgent, (s, p) => s === 200 && p.id === "perm-agent");
await step("permission section round-trips", "GET", "/v1/config/agents/perm-agent", undefined, (s, p) =>
  s === 200 && p.plugin_config?.permission?.default_behavior === "deny" &&
  p.plugin_config?.permission?.rules?.[0]?.pattern === "Bash(*rm*)");
await step("publish permission agent (compiles the section)", "POST", "/v1/config/agents/perm-agent/publish", undefined, (s, p) => s === 200 && p.installed === true);
// Gating truth: the Observe faces are genuinely unmounted, so GatedPage's probe
// gets a 404 and shows the placeholder (not a fabricated flag).
await step("gated Observe face 404s (audit-log)", "GET", "/v1/audit-log", undefined, (s) => s === 404 || s === 405);

// ---- Managed resources: memory / skills / environments / deployments ----
// The surfaces (memory.tsx / skills.tsx / environments.tsx / deployments.tsx) drive
// these; the create→list round-trip proves the console↔endpoint wiring.
const mem = await step("create memory store", "POST", "/v1/memory_stores", { name: "smoke-mem" }, (s, p) => (s === 200 || s === 201) && typeof p.id === "string");
await step("list memory stores", "GET", "/v1/memory_stores", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === mem.id));
// A File input and a Skill capability use distinct lifecycle/configuration paths.
const file = await uploadStep("upload file", "/v1/files", "notes.txt", "text/plain", "the port is 8080", { purpose: "agent" }, (s, p) => s === 200 && typeof p.id === "string");
const skill = await uploadStep("create skill", "/v1/skills", "SKILL.md", "text/markdown", "# Greeter\nSay hello.", { name: "smoke-skill" }, (s, p) => s === 200 && typeof p.id === "string");
await step("list skills (durable store)", "GET", "/v1/skills", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === skill.id));
await step("bind typed inputs to agent (memory/file)", "PUT", "/v1/config/agents/smoke-agent/resources", {
  agent_id: "smoke-agent",
  inputs: [
    { binding_id: "memory", target: { kind: "memory_store", id: mem.id }, mount_path: "/mnt/memory", access: "read_write" },
    { binding_id: "file", target: { kind: "file", id: file.id }, mount_path: "/mnt/files/notes.txt", access: "read_only" },
  ],
  revision: 1,
}, (s, p) => s === 200 && p.inputs.length === 2);
await step("agent inputs round-trip (typed identities)", "GET", "/v1/config/agents/smoke-agent/resources", undefined, (s, p) =>
  s === 200 &&
  ["memory_store", "file"].every((k) => p.inputs.some((r) => r.target.kind === k)) &&
  p.inputs.find((r) => r.target.kind === "file")?.target.id === file.id);
const env = await step("create environment", "POST", "/v1/environments", { name: "smoke-env", config: { type: "cloud", networking: { type: "unrestricted" } } }, (s, p) => (s === 200 || s === 201) && typeof p.id === "string");
await step("list environments", "GET", "/v1/environments", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === env.id));
// A deployment binds a published agent (smoke-agent, above) to an environment + schedule.
const dep = await step("create deployment", "POST", "/v1/deployments", {
  name: "smoke-dep",
  agent: "smoke-agent",
  environment_id: env.id,
  schedule: { type: "cron", expression: "0 20 * * 5", timezone: "UTC" },
  initial_events: [{ type: "user.message", content: [{ type: "text", text: "run" }] }],
}, (s, p) => (s === 200 || s === 201) && typeof p.id === "string");
await step("list deployments", "GET", "/v1/deployments", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === dep.id));

// ---- Workspace · Runtime secrets (surfaces/vaults.tsx) ----
const vault = await step("create vault", "POST", "/v1/vaults", { display_name: "smoke" });
await step("vault credential (static_bearer)", "POST", `/v1/vaults/${vault.id}/credentials`, {
  type: "static_bearer",
  mcp_server_url: "https://mcp.example.com/docs",
  token: "test-token", // awaken-allow: secret (synthetic smoke fixture)
});

// ---- Sessions on the flat data plane (surfaces/sessions.tsx via ws()) ----
// Tenancy is an edge aspect: the flat `/v1/sessions` surface runs under the
// seeded DEFAULT_SCOPE (Option A, single-tenant). The `/projects/{id}` ingress is
// retired (ADR-0048 supersedes ADR-0042); scope now comes from the key or the
// `/v1/workspaces/{ws}/…` path form (probed below).
const session = await step("create session (flat)", "POST", "/v1/sessions", {
  agent: "default",
  title: "smoke",
  vault_ids: [vault.id],
}, (s, p) => s === 200 || s === 201 ? typeof p.id === "string" : false);
// The session carries an accumulated `usage` object (input/output/cache tokens) —
// the Sandbox/Test UsageBadges read it. Zero until the first turn commits.
await step("retrieve session (usage shape)", "GET", `/v1/sessions/${session.id}`, undefined, (s, p) =>
  s === 200 && p.id === session.id && p.usage && typeof p.usage.input_tokens === "number");
await step("list events", "GET", `/v1/sessions/${session.id}/events`, undefined, (s, p) => s === 200 && Array.isArray(p.data));
await step("list sessions", "GET", "/v1/sessions", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === session.id));
const renamed = await step("rename session", "POST", `/v1/sessions/${session.id}`, { title: "smoke (renamed)" }, (s, p) => s === 200 && p.title === "smoke (renamed)");
await step("archive session", "POST", `/v1/sessions/${renamed.id}/archive`, undefined, (s, p) => s === 200 && typeof p.archived_at === "string");
await step("archived row stays listed", "GET", "/v1/sessions", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === session.id && x.archived_at));
// Workspace path addressing (ADR-0048): `/v1/workspaces/{ws}/sessions` is rewritten
// to flat `/v1/sessions` and scoped to {ws} — the ws() seam's target for Option B.
await step("workspace-path session list", "GET", "/v1/workspaces/default/sessions", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === session.id));

console.log(failures === 0 ? "\nSMOKE OK" : `\nSMOKE FAILED (${failures})`);
directory.close();
process.exit(failures === 0 ? 0 : 1);
