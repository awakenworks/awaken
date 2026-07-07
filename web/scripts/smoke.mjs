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
  flavor: "anthropic_messages",
  base_url: null,
  timeout_secs: 60,
  display_name: "Anthropic Messages",
  version: 1,
});
await step("author offering", "POST", "/v1/config/offerings", {
  model_id: "claude-sonnet-4-5",
  provider_id: "anthropic",
  protocol_endpoint_id: "anthropic-messages",
  flavor: "anthropic_messages",
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
await step("author project", "PUT", "/v1/config/projects/demo", {
  id: "demo",
  workspace_id: "wrkspc_default",
  display_name: "Demo project",
  version: 1,
});
await step("list projects", "GET", "/v1/config/projects", undefined, (s, p) => s === 200 && p.some((x) => x.id === "demo"));

// ---- Project · agent MCP binding (surfaces/project-agents.tsx) ----
await step("bind project agent mcp", "PUT", "/v1/config/projects/demo/agents/default/mcp", {
  project_id: "demo",
  agent_id: "default",
  mcp_server_ids: ["docs-search"],
  version: 1,
});
await step("read project agent mcp", "GET", "/v1/config/projects/demo/agents/default/mcp", undefined, (s, p) => s === 200 && p.mcp_server_ids.includes("docs-search"));

// ---- Project · Agents authoring via config plane (surfaces/agent-editor.tsx) ----
// The console authors the rich AgentConfig against our own management API, then
// publishes (compile + install). Publish/compile is inference-agnostic.
const cfgAgent = {
  id: "smoke-agent",
  instructions: "You are a smoke-test agent.",
  max_steps: 8,
  model_binding: { provider_instance_ref: "anthropic", model_ref: "claude", backend_ref: "anthropic_messages" },
  tool_ids: [],
  plugin_ids: [],
  plugin_config: {},
  context_policy: { kind: "keep_all" },
};
await step("author config agent", "PUT", "/v1/config/agents/smoke-agent", cfgAgent, (s, p) => s === 200 && p.id === "smoke-agent");
await step("validate config agent", "POST", "/v1/config/agents/smoke-agent/validate", cfgAgent, (s, p) => s === 200 && p.valid === true);
await step("list config agents (draft)", "GET", "/v1/config/agents", undefined, (s, p) => s === 200 && p.data.some((a) => a.id === "smoke-agent" && a.published === false));
await step("publish config agent", "POST", "/v1/config/agents/smoke-agent/publish", undefined, (s, p) => s === 200 && p.installed === true);
await step("list config agents (published)", "GET", "/v1/config/agents", undefined, (s, p) => s === 200 && p.data.some((a) => a.id === "smoke-agent" && a.published === true));

// ---- Project · Vaults (surfaces/vaults.tsx; bare face until §7.10) ----
const vault = await step("create vault", "POST", "/v1/vaults", { display_name: "smoke" });
await step("vault credential (static_bearer)", "POST", `/v1/vaults/${vault.id}/credentials`, {
  type: "static_bearer",
  mcp_server_url: "https://mcp.example.com/docs",
  token: "test-token", // awaken-allow: secret (synthetic smoke fixture)
});

// ---- Project · Sessions via ingress (surfaces/sessions.tsx) ----
const session = await step("create session via /projects/demo", "POST", "/projects/demo/v1/sessions", {
  agent: "default",
  title: "smoke",
  vault_ids: [vault.id],
}, (s, p) => s === 200 || s === 201 ? typeof p.id === "string" : false);
await step("retrieve session via ingress", "GET", `/projects/demo/v1/sessions/${session.id}`, undefined, (s, p) => s === 200 && p.id === session.id);
await step("list events via ingress", "GET", `/projects/demo/v1/sessions/${session.id}/events`, undefined, (s, p) => s === 200 && Array.isArray(p.data));
await step("list sessions via ingress", "GET", "/projects/demo/v1/sessions", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === session.id));
await step("workspace-wide session list", "GET", "/v1/sessions", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === session.id));
const renamed = await step("rename session", "POST", `/projects/demo/v1/sessions/${session.id}`, { title: "smoke (renamed)" }, (s, p) => s === 200 && p.title === "smoke (renamed)");
await step("archive session", "POST", `/projects/demo/v1/sessions/${renamed.id}/archive`, undefined, (s, p) => s === 200 && typeof p.archived_at === "string");
await step("archived row stays listed", "GET", "/projects/demo/v1/sessions", undefined, (s, p) => s === 200 && p.data.some((x) => x.id === session.id && x.archived_at));
await step("unknown project 404s (managed envelope)", "GET", "/projects/ghost/v1/sessions/x", undefined, (s, p) => s === 404 && p?.error?.type === "not_found_error");

console.log(failures === 0 ? "\nSMOKE OK" : `\nSMOKE FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);
