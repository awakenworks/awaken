// Configuration-to-execution acceptance: production Control/Coordinator/Worker,
// official Anthropic SDK, real MCP HTTP and SDK stdio servers. --live selects
// real DeepSeek; the default uses the existing deterministic provider fixture.
// Cause/effect graph: URL identity + Workspace + active credential -> matching
// candidates; explicit choice + publication -> exact account; Session Vaults
// cannot replace a pinned account. Configuration CAS + valid cloud revision ->
// one commit. Rejected selection/denied tool -> no invocation. Restart -> same
// definitions/defaults/publications. Vault.type remains "vault" for mixed auth.
// Rules C1 two accounts/same URL, C2 foreign target, C3 anonymous/bearer/OAuth,
// C4 fixed account versus another Session Vault, C5 denial, C6 retirement,
// C7 cloud replay/conflict/malformed/offline, C8 local stdio without implicit
// Vault-to-process secret projection, C9 restart.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import { randomBytes } from 'node:crypto';
import { createRequire } from 'node:module';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { spawnProduction, stopServer, waitForPort, waitForSessionEventReceipt, provisionProviderTestModel, cleanupFixtureTree, FILES_BETA } from './harness.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { startTemplateMcp } from './fixtures/template_mcp_fixture.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { closeHttpServer } from './http_server.mjs';

const live = process.argv.includes('--live');
const PORT = Number(process.env.E2E_PORT);
const base = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01', FILES_BETA];
const directory = path.resolve(process.env.AWAKEN_EVAL_ARTIFACT_DIR ?? fs.mkdtempSync(path.join(os.tmpdir(), 'mcp-config-')));
const data = path.join(directory, 'server-data');
const tokenA = randomBytes(24).toString('hex'), tokenB = randomBytes(24).toString('hex');
const marker = randomBytes(16).toString('hex');
let fixture, anonymous, upstream, server, cloud;
const evidence = [];
let snapshot, malformed = false;
const definition = (id, url) => ({ id, name: id, description: 'Credential association test', icon: null, transport: { type: 'url', url }, prompts_as_skills: false });
async function config(method, route, body, expected = 200) {
  const response = await fetch(`${base}/v1/config${route}`, { method, headers: { 'content-type': 'application/json' }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
  const text = await response.text();
  assert.ok(!text.includes(tokenA) && !text.includes(tokenB), 'no credential material in config responses');
  assert.equal(response.status, expected, `${method} ${route}: ${text.slice(0, 800)}`);
  return text ? JSON.parse(text) : null;
}
const launch = () => spawnProduction(data, PORT, { stdout: 'inherit', fields: { sandbox_tier: 'namespace', log_filter: 'warn' } }, withoutAmbientProviderKeys(process.env));
const clientFor = () => new Anthropic({ apiKey: 'e2e-local', baseURL: base, maxRetries: 0 }); // awaken-allow: secret (test service)
async function saveServer(definition) {
  const current = await config('GET', '/mcp-servers');
  return config('PUT', '/mcp-servers', { expected_revision: current.revision, definition });
}
async function select(key, credential_id, operation = 'prepare', expected = 200) {
  const current = await config('GET', '/mcp-servers');
  return config('POST', `/mcp-servers/${encodeURIComponent(key)}/${operation}`, { expected_revision: current.revision, credential_id }, expected);
}
async function publish(binding, model, ask = false) {
  const id = `mcp-config-${randomBytes(6).toString('hex')}`;
  const saved = await config('PUT', `/agents/${id}`, {
    id, name: 'MCP credential proof', model: { id: model }, system: 'Call the declared read_source tool once, then report credential_account and marker exactly as returned. Do not invent successful calls. If denied, report denial and stop.',
    mcp_servers: [binding], tools: [{ type: 'mcp_toolset', mcp_server_name: binding.name, default_config: { permission_policy: { type: ask ? 'always_ask' : 'always_allow' } } }],
  });
  const published = await config('POST', `/agents/${id}/publish`, { source_revision: saved.generation });
  return { type: 'agent', id, version: published.source_revision };
}
async function run(client, agent, label, { vaultIds = [], resources = [], deny = false } = {}) {
  const before = fixture.state.calls.length;
  const session = await client.beta.sessions.create({ agent, environment_id: 'env_local', vault_ids: vaultIds, resources, betas: BETAS });
  const receipt = (await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: 'Use read_source once and report the exact credential_account and marker from its response.' }] }], betas: BETAS })).data[0];
  let result = await waitForSessionEventReceipt(client, session.id, receipt.id, BETAS, ({ delta }) => delta.some((e) => e.type === 'session.status_idle' || e.type === 'session.status_terminated'), label, { timeoutMs: 180_000 });
  fs.writeFileSync(path.join(directory, `${label}-events.json`), JSON.stringify(result.events, null, 2));
  assert.ok(!result.events.some((e) => e.type === 'session.status_terminated'), `${label}: ${JSON.stringify(result.events.filter((e) => e.type === 'session.error'))}`);
  if (deny) {
    assert.equal(result.delta.filter((e) => e.type === 'session.status_idle').at(-1).stop_reason.type, 'requires_action');
    assert.equal(fixture.state.calls.length, before, 'C5 no pre-approval invocation');
    const tool = result.delta.find((e) => e.type === 'agent.mcp_tool_use'); assert.ok(tool);
    const denied = (await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.tool_confirmation', tool_use_id: tool.id, result: 'deny' }], betas: BETAS })).data[0];
    result = await waitForSessionEventReceipt(client, session.id, denied.id, BETAS, ({ delta }) => delta.some((e) => e.type === 'session.status_idle' && e.stop_reason.type === 'end_turn'), label, { timeoutMs: 180_000 });
    assert.equal(fixture.state.calls.length, before, 'C5 denied call never executes');
  } else {
    assert.equal(result.delta.filter((e) => e.type === 'session.status_idle').at(-1).stop_reason.type, 'end_turn');
    assert.ok(JSON.stringify(result.events.filter((e) => e.type === 'agent.message')).includes(marker), 'actual source marker reaches the answer');
  }
  fs.writeFileSync(path.join(directory, `${label}-events.json`), JSON.stringify(result.events, null, 2));
  evidence.push({ label, session_id: session.id, pass: true, calls: fixture.state.calls.slice(before) });
  console.log(`PASS ${label}`);
  return session;
}
async function main() {
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  fixture = await startTemplateMcp({ tools: { accounts: ['read_source'] }, records: { accounts: [{ marker }] } }, { accounts: { [tokenA]: 'Product', [tokenB]: 'Personal' } });
  // Reuse the same SDK fixture for anonymous transport coverage.
  anonymous = await startTemplateMcp({ tools: { public: ['read_source'] }, records: { public: [{ marker }] } }, { accounts: { '': 'Public' } });
  snapshot = { schema_version: 1, revision: 10, servers: [definition('cloud-example', fixture.servers[0].url)] };
  cloud = http.createServer((_req, res) => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(malformed ? '{invalid' : JSON.stringify(snapshot)); });
  await new Promise((resolve) => cloud.listen(0, '127.0.0.1', resolve));
  server = launch(); await waitForPort(PORT, 30_000);
  const context = await fetch(`${base}/v1/config/workspace-context`); const workspace = context.headers.get('anthropic-workspace-id'); assert.ok(workspace);
  let key, model, endpoint, provider, dialect;
  if (live) {
    key = process.env.DEEPSEEK_API_KEY ?? (process.env.DEEPSEEK_API_KEY_FILE && fs.readFileSync(process.env.DEEPSEEK_API_KEY_FILE, 'utf8').trim());
    assert.ok(key, '--live requires DeepSeek credentials'); model = process.env.DEEPSEEK_MODEL ?? 'deepseek-flash'; endpoint = 'https://api.deepseek.com'; provider = 'deepseek'; dialect = 'open_ai_chat';
  } else {
    key = 'mcp-config-fixture-key'; // awaken-allow: secret
    model = 'mcp-config-fixture'; provider = 'anthropic'; dialect = 'anthropic_messages';
    upstream = await startFakeAnthropic(key, { models: [model], behavior: (request) => {
      const blocks = (request.messages ?? []).flatMap((m) => Array.isArray(m.content) ? m.content : []);
      const results = blocks.filter((b) => b.type === 'tool_result');
      if (results.length) return { text: JSON.stringify(results.at(-1)) };
      const tool = request.tools?.find((t) => t.name.includes('read_source')); assert.ok(tool);
      return { tool: { id: 'read-once', name: tool.name, input: {} } };
    } }); endpoint = `${upstream.url}/v1`;
  }
  const supplied = await provisionProviderTestModel(base, workspace, { id: 'mcp-config', provider_id: provider, dialect, base_url: endpoint, secret: key, model_id: model, timeout_secs: 180 });
  const client = clientFor();
  const va = await client.beta.vaults.create({ display_name: 'Product Vault', betas: BETAS });
  const vb = await client.beta.vaults.create({ display_name: 'Personal Vault', betas: BETAS });
  assert.equal(va.type, 'vault');
  const ca = await client.beta.vaults.credentials.create(va.id, { display_name: 'Product Notion', auth: { type: 'static_bearer', mcp_server_url: fixture.servers[0].url, token: tokenA }, betas: BETAS });
  const cb = await client.beta.vaults.credentials.create(vb.id, { display_name: 'Personal Notion', auth: { type: 'mcp_oauth', mcp_server_url: fixture.servers[0].url, access_token: tokenB }, betas: BETAS });
  // A mixed Vault is legal: resource type is not an auth category.
  await client.beta.vaults.credentials.create(va.id, { auth: { type: 'environment_variable', networking: { type: 'limited', allowed_hosts: ['example.test'] }, secret_name: 'MCP_CONFIG_TEST', secret_value: 'synthetic-env-value' }, betas: BETAS }); // awaken-allow: secret
  await saveServer(definition('accounts', fixture.servers[0].url));
  await saveServer(definition('public', anonymous.servers[0].url));
  const candidates = await config('GET', '/mcp-servers/local:accounts/credentials');
  assert.deepEqual(candidates.data.map((c) => c.id).sort(), [ca.id, cb.id].sort(), 'C1 both matching accounts, no env credential');
  for (const id of [ca.id, cb.id]) {
    const tested = await select('local:accounts', id, 'inspect');
    assert.equal(tested.status, 'valid'); assert.equal(tested.credential.id, id); assert.equal(tested.tools[0].name, 'read_source');
  }
  assert.equal((await select('local:public', null, 'inspect')).status, 'valid', 'C3 anonymous MCP');
  await select('local:public', ca.id, 'inspect', 400);
  let current = await config('GET', '/mcp-servers');
  await config('PUT', '/mcp-servers/local:accounts/default-credential', { expected_revision: current.revision, credential_id: cb.id });
  const prepared = await select('local:accounts', cb.id);
  const pinned = await publish(prepared.binding, supplied.model);
  await run(client, pinned, 'pinned-personal-with-product-vault', { vaultIds: [va.id] });
  assert.equal(evidence.at(-1).calls[0].account, 'Personal', 'C4 pinned exact account wins');
  const { credential: _credential, ...unbound } = prepared.binding;
  const portable = await publish(unbound, supplied.model);
  await run(client, portable, 'standard-ordered-vaults', { vaultIds: [va.id, vb.id] });
  assert.equal(evidence.at(-1).calls[0].account, 'Product', 'standard Vault order unchanged');
  await run(client, await publish(prepared.binding, supplied.model, true), 'denied-account-call', { deny: true });
  // The cloud feed cannot overwrite local definitions or defaults.
  current = await config('GET', '/mcp-servers');
  const sourceUrl = `http://127.0.0.1:${cloud.address().port}/catalog.json`;
  await config('PUT', '/mcp-catalog/source', { expected_revision: current.revision, url: sourceUrl });
  const synced = await config('POST', '/mcp-catalog/sync');
  assert.ok(synced.servers.some((s) => s.key === 'local:accounts'));
  assert.equal(synced.servers.find((s) => s.key === 'local:accounts').default_credential.vault_credential_id, cb.id);
  assert.equal((await config('POST', '/mcp-catalog/sync')).revision, synced.revision, 'C7 exact replay is a no-op');
  snapshot.servers[0].name = 'conflicting revision'; await config('POST', '/mcp-catalog/sync', undefined, 409);
  malformed = true; await config('POST', '/mcp-catalog/sync', undefined, 400); malformed = false;
  assert.equal((await config('GET', '/mcp-servers')).revision, synced.revision);
  // SDK stdio fixture is bundled once for the actual Namespace, not emulated in
  // the runtime. A read-only uploaded file holds the server, invoked by node.
  const require = createRequire(import.meta.url); const webRequire = createRequire(new URL('../web/package.json', import.meta.url));
  const esbuild = createRequire(webRequire.resolve('vite'))('esbuild');
  const bundle = await esbuild.build({ stdin: { contents: `const {McpServer}=require(${JSON.stringify(require.resolve('@modelcontextprotocol/sdk/server/mcp.js'))}); const {StdioServerTransport}=require(${JSON.stringify(require.resolve('@modelcontextprotocol/sdk/server/stdio.js'))}); const s=new McpServer({name:'stdio-proof',version:'1'}); s.registerTool('read_source',{inputSchema:{},description:'Read the test record'},async()=>({content:[{type:'text',text:JSON.stringify({credential_account:'Sandbox stdio',marker:${JSON.stringify(marker)}})}]})); s.connect(new StdioServerTransport()).catch(()=>process.exit(1));`, resolveDir: process.cwd() }, bundle: true, platform: 'node', format: 'cjs', write: false });
  const bundlePath = path.join(directory, 'mcp-test.cjs'); fs.writeFileSync(bundlePath, bundle.outputFiles[0].contents);
  const fixtureClient = new Client({ name: 'bundle-preflight', version: '1' });
  await fixtureClient.connect(new StdioClientTransport({ command: '/usr/bin/node', args: [bundlePath] }));
  assert.equal((await fixtureClient.listTools()).tools[0].name, 'read_source'); await fixtureClient.close();
  const file = await client.beta.files.upload({ file: await toFile(Buffer.from(bundle.outputFiles[0].contents), 'mcp-test.cjs'), betas: BETAS });
  await saveServer({ ...definition('stdio', ''), transport: { type: 'sandbox_stdio', command: '/usr/bin/node', args: ['/mnt/session/uploads/mcp-test.cjs'] } });
  const stdio = await select('local:stdio', null);
  await run(client, await publish(stdio.binding, supplied.model), 'namespace-stdio', { resources: [{ type: 'file', file_id: file.id, mount_path: '/mnt/session/uploads/mcp-test.cjs' }] });
  await select('local:stdio', ca.id, 'prepare', 400);
  const beforeRestart = await config('GET', '/mcp-servers');
  await stopServer(server); server = undefined;
  await closeHttpServer(cloud); cloud = undefined;
  server = launch(); await waitForPort(PORT, 30_000);
  const restored = await config('GET', '/mcp-servers');
  assert.deepEqual(restored, beforeRestart, 'C9 offline restart preserves config/defaults/catalog');
  await run(clientFor(), pinned, 'pinned-account-after-restart', { vaultIds: [va.id] });
  assert.equal(evidence.at(-1).calls[0].account, 'Personal');
  await clientFor().beta.vaults.credentials.archive(cb.id, { vault_id: vb.id, betas: BETAS });
  await select('local:accounts', cb.id, 'prepare', 400);
  fs.writeFileSync(path.join(directory, 'mcp-configuration-evidence.json'), JSON.stringify({ pass: true, live, model, cases: evidence, checks: ['scope-and-target', 'mixed-vault-types', 'bearer-oauth-anonymous', 'exact-account', 'denial', 'catalog-cas-offline', 'namespace-stdio', 'restart', 'retirement'] }, null, 2));
  console.log('E2E PASS: MCP configuration, credential association, cloud cache and real execution.');
}
try { await main(); } catch (error) { console.error(`E2E FAIL: ${error.stack}`); process.exitCode = 1; }
finally { if (server) await stopServer(server); if (fixture) await fixture.close(); if (anonymous) await anonymous.close(); if (upstream) await upstream.close(); if (cloud) await closeHttpServer(cloud); cleanupFixtureTree(data); }
