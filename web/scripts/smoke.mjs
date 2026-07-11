#!/usr/bin/env node
// Console ⇄ backend integration smoke: replays the exact request sequence the
// surfaces issue (same paths, same bodies) against a management-mode host.
//   AWAKEN_HTTP_URL=http://127.0.0.1:38091 node web/scripts/smoke.mjs

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

// ---- Workspace · Models (surfaces/models.tsx) ----
await step("author provider", "PUT", "/v1/config/providers/anthropic", {
  id: "anthropic",
  slug: "anthropic",
  display_name: "Anthropic",
  version: 1,
});
await step("author endpoint", "PUT", "/v1/config/endpoints/anthropic-messages", {
  id: "anthropic-messages",
  provider_id: "anthropic",
  dialect: "anthropic_messages",
  base_url: null,
  timeout_secs: 60,
  display_name: "Anthropic Messages",
  version: 1,
});
await step("author offering", "POST", "/v1/config/offerings", {
  model_id: "claude-sonnet-4-5",
  provider_id: "anthropic",
  protocol_endpoint_id: "anthropic-messages",
  dialect: "anthropic_messages",
  upstream_model: null,
});
await step("catalog snapshot", "GET", "/v1/config/catalog", undefined, (s, p) => s === 200 && p.offerings.length >= 1);

// ---- Workspace · Credentials (surfaces/credentials.tsx) ----
const cred = await step("enter credential", "POST", "/v1/config/credentials", {
  workspace_id: "wrkspc_default",
  kind: "vault",
  provider_id: "anthropic",
  secret: "sk-test-not-a-real-key", // awaken-allow: secret (synthetic smoke fixture)
});
await step("list credentials", "GET", "/v1/config/credentials?workspace_id=wrkspc_default", undefined, (s, p) => s === 200 && Array.isArray(p) && p.length >= 1);
await step("resolve dry-run (exact)", "POST", "/v1/config/inference/resolve", {
  workspace_id: "wrkspc_default",
  model_id: "claude-sonnet-4-5",
  binding: { type: "exact", credential_source_id: cred.id },
}, (s, p) => s === 200 && p.credential_present === true);

// ---- Workspace · MCP (surfaces/mcp-servers.tsx) ----
await step("author mcp server", "PUT", "/v1/config/mcp-servers/docs-search", {
  id: "docs-search",
  display_name: "Docs search",
  url: "https://mcp.example.com/docs",
  credential_binding: { type: "exact", credential_source_id: cred.id },
  version: 1,
});
await step("list mcp servers", "GET", "/v1/config/mcp-servers", undefined, (s, p) => s === 200 && p.length >= 1);

// ---- Settings: project authoring (surfaces/settings.tsx) ----
// Project was removed from the tenancy model (ADR-0051 `remove Project` +
// ADR-0048): there is no `/v1/config/projects` resource and no `/projects/{id}`
// ingress; the tenant is the workspace (addressed by key or `/v1/workspaces/{ws}/…`).

// ---- Agent MCP binding on the flat config plane (surfaces/agent-editor.tsx) ----
await step("bind agent mcp", "PUT", "/v1/config/agents/default/mcp", {
  agent_id: "default",
  mcp_server_ids: ["docs-search"],
  version: 1,
});
await step("read agent mcp", "GET", "/v1/config/agents/default/mcp", undefined, (s, p) => s === 200 && p.mcp_server_ids.includes("docs-search"));

// ---- Project · Agents authoring via config plane (surfaces/agent-editor.tsx) ----
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
  mcp_servers: [],
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
// A file blob (Files API) and a skill (durable skill store, now wired in management
// mode) — the other resource kinds an agent can bind (ADR-0038).
const file = await uploadStep("upload file", "/v1/files", "notes.txt", "text/plain", "the port is 8080", { purpose: "agent" }, (s, p) => s === 200 && typeof p.id === "string");
const skill = await uploadStep("create skill", "/v1/skills", "SKILL.md", "text/markdown", "# Greeter\nSay hello.", { name: "smoke-skill" }, (s, p) => s === 200 && typeof p.id === "string");
await step("list skills (durable store)", "GET", "/v1/skills", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === skill.id));
// Bind ALL FOUR kinds to the published agent (ADR-0038 agent resources) — the loop
// that makes each resource usable: authored here → read into resource prompts + mounts
// at compile, and staged into the sandbox at session-create (the config service and the
// runtime host share this same resource store).
await step("bind resources to agent (memory/file/repo/skill)", "PUT", "/v1/config/agents/smoke-agent/resources", {
  agent_id: "smoke-agent",
  resources: [
    { kind: "memory_store", resource_id: mem.id, mount_path: "/mnt/memory", access: "read_write" },
    { kind: "file", resource_id: file.id, mount_path: "/mnt/files/notes.txt", access: "read_only" },
    { kind: "github_repository", resource_id: "https://github.com/awaken/example.git", mount_path: "/mnt/repo", access: "read_only" },
    { kind: "skill", resource_id: skill.id, mount_path: "/mnt/skills/greeter", access: "read_only" },
  ],
  version: 1,
}, (s, p) => s === 200 && p.resources.length === 4);
await step("agent resources round-trip (all kinds)", "GET", "/v1/config/agents/smoke-agent/resources", undefined, (s, p) =>
  s === 200 &&
  ["memory_store", "file", "github_repository", "skill"].every((k) => p.resources.some((r) => r.kind === k)) &&
  p.resources.find((r) => r.kind === "file")?.resource_id === file.id);
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

// ---- Project · Vaults (surfaces/vaults.tsx; bare face until §7.10) ----
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
process.exit(failures === 0 ? 0 : 1);
