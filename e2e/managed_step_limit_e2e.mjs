// Default-limit conformance through official SDK and a real production service.
// R1 standard create + 24 dependent MCP reads -> >16 model requests and end_turn.
// R2 explicit config-plane limit + Managed update -> private limit preserved,
// official projection unchanged, old publication still executes without ceiling.
// R3 process restart -> frozen versions/history preserved, no replayed effects.
// R4 finite limit -> at most N model requests; deterministic model keeps asking
// for tools so the public terminal is retries_exhausted rather than success.
// Real mode: --live + DEEPSEEK_API_KEY(_FILE) uses DeepSeek.
// Deterministic mode reuses the canonical wire-provider fixture; no fake runtime.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { spawnProduction, stopServer, waitForPort, waitForSessionEventReceipt,
  provisionProviderTestModel, cleanupFixtureTree } from './harness.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { startTemplateMcp } from './fixtures/template_mcp_fixture.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const live = process.argv.includes('--live');
const PORT = Number(process.env.E2E_PORT);
const directory = path.resolve(process.env.AWAKEN_EVAL_ARTIFACT_DIR ?? fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-step-limit-')));
const nodes = Array.from({ length: 24 }, () => randomBytes(10).toString('hex'));
const answer = randomBytes(12).toString('hex');
const records = nodes.map((id, i) => ({ id, next_id: nodes[i + 1] ?? null, ...(i === 23 ? { answer } : {}) }));
let upstream, fixture, server;
const rows = [];
const data = path.join(directory, 'server-data');
const launch = () => spawnProduction(data, PORT, { stdout: 'inherit', fields: { sandbox_tier: 'namespace', log_filter: 'warn' } }, withoutAmbientProviderKeys(process.env));
const clientFor = () => new Anthropic({ apiKey: 'e2e-local', baseURL: `http://127.0.0.1:${PORT}`, maxRetries: 0 }); // awaken-allow: secret (local no-login placeholder)
const config = async (method, route, body) => {
  const response = await fetch(`http://127.0.0.1:${PORT}/v1/config${route}`, {
    method, headers: { 'content-type': 'application/json' }, ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const value = await response.json(); assert.ok(response.ok, `${method} ${route}: ${response.status}`); return value;
};
async function allEvents(client, id) {
  const events = []; for await (const event of client.beta.sessions.events.list(id, { betas: BETAS })) events.push(event); return events;
}
async function run(client, agent, vault, label, finite = false) {
  const start = fixture.state.calls.length;
  const session = await client.beta.sessions.create({ agent: { type: 'agent', id: agent.id, version: agent.version }, environment_id: 'env_local', vault_ids: [vault.id], betas: BETAS });
  const receipt = (await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: `Follow the record chain starting at id ${nodes[0]}. Call chain.read_source with exactly that id, then the returned next_id, one call per response. Never read without an id. Continue until next_id is null, then reply with the final answer value. Do not stop early or ask the user to continue.` }] }], betas: BETAS })).data[0];
  const { events, delta } = await waitForSessionEventReceipt(client, session.id, receipt.id, BETAS,
    ({ delta }) => delta.some((e) => e.type === 'session.status_idle'), label, { timeoutMs: 300_000 });
  const modelRequests = delta.filter((e) => e.type === 'span.model_request_end').length;
  const reads = fixture.state.calls.slice(start).filter((c) => c.name === 'read_source');
  const stop = delta.filter((e) => e.type === 'session.status_idle').at(-1).stop_reason.type;
  if (finite) {
    assert.ok(modelRequests <= 5 && modelRequests > 0, 'R4 finite upper bound');
    assert.ok(reads.length < nodes.length, 'R4 chain was not completed');
    assert.ok(!JSON.stringify(delta.filter((e) => e.type === 'agent.message')).includes(answer), 'R4 no fabricated completion');
    if (!live) assert.equal(stop, 'retries_exhausted', 'R4 exhausted is not successful');
  } else {
    assert.equal(stop, 'end_turn');
    assert.ok(modelRequests >= 25, `R1 expected >16 inference steps; observed ${modelRequests}`);
    assert.deepEqual(reads.map((c) => c.input.id), nodes, 'R1 each dependent source was read in order');
    assert.ok(JSON.stringify(delta.filter((e) => e.type === 'agent.message')).includes(answer), 'R1 final answer came from the last source');
  }
  const row = { label, session_id: session.id, version: agent.version, model_requests: modelRequests, mcp_reads: reads.length, stop, task_complete: !finite, pass: true };
  rows.push(row);
  fs.writeFileSync(path.join(directory, `${label}-events.json`), JSON.stringify(events, null, 2));
  console.log(`PASS ${label}: ${modelRequests} model requests, ${reads.length} MCP reads, ${stop}`);
  return row;
}
async function main() {
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  let key, model, endpoint, provider, dialect;
  if (live) {
    key = process.env.DEEPSEEK_API_KEY ?? (process.env.DEEPSEEK_API_KEY_FILE && fs.readFileSync(process.env.DEEPSEEK_API_KEY_FILE, 'utf8').trim());
    assert.ok(key, 'real DeepSeek credentials required; never skip');
    model = process.env.DEEPSEEK_MODEL ?? 'deepseek-flash'; endpoint = 'https://api.deepseek.com'; provider = 'deepseek'; dialect = 'open_ai_chat';
  } else {
    key = 'step-limit-fixture-key'; // awaken-allow: secret (synthetic upstream)
    model = 'step-limit-fixture'; provider = 'anthropic'; dialect = 'anthropic_messages';
    upstream = await startFakeAnthropic(key, { models: [model], behavior: (request) => {
      const calls = (request.messages ?? []).flatMap((m) => Array.isArray(m.content) ? m.content : []).filter((b) => b.type === 'tool_use');
      if (calls.length >= nodes.length) return { text: answer };
      const name = request.tools?.find((t) => t.name.includes('read_source'))?.name ?? calls.at(-1)?.name;
      assert.ok(name, 'runtime advertises the MCP tool');
      return { tool: { id: `chain-${calls.length}`, name, input: { id: nodes[calls.length] } } };
    } });
    endpoint = `${upstream.url}/v1`;
  }
  fixture = await startTemplateMcp({ tools: { chain: ['read_source'] }, records: { chain: records } });
  server = launch(); await waitForPort(PORT, 30_000);
  const base = `http://127.0.0.1:${PORT}`;
  const context = await fetch(`${base}/v1/config/workspace-context`);
  assert.ok(context.ok); const workspace = context.headers.get('anthropic-workspace-id'); assert.ok(workspace);
  const published = await provisionProviderTestModel(base, workspace, { id: 'step-limit', provider_id: provider, dialect, base_url: endpoint, secret: key, model_id: model, timeout_secs: 180 });
  const client = clientFor();
  const vault = await client.beta.vaults.create({ display_name: 'chain fixture', betas: BETAS });
  await client.beta.vaults.credentials.create(vault.id, { auth: { type: 'static_bearer', mcp_server_url: fixture.servers[0].url, token: fixture.token }, betas: BETAS });
  const agent = await client.beta.agents.create({ name: 'Long dependent read', model: published.model, system: 'Complete the full chain using only the declared MCP read tool. Each next id is available only in the preceding result. Do not invent ids or answers.', mcp_servers: fixture.servers,
    tools: [{ type: 'mcp_toolset', mcp_server_name: 'chain', default_config: { permission_policy: { type: 'always_allow' } } }], betas: BETAS });
  assert.ok(!Object.hasOwn(agent, 'max_steps'));
  assert.equal((await config('GET', `/agents/${agent.id}`)).max_steps, null, 'R1 canonical default');
  await run(client, agent, vault, 'standard-default');
  // Only this private-control case uses Config authoring; every execution and
  // observation still uses standard Managed APIs. No per-Session hidden override.
  const existing = await config('GET', `/agents/${agent.id}`);
  const saved = await config('PUT', `/agents/${agent.id}`, { ...existing, max_steps: 5 });
  await config('POST', `/agents/${agent.id}/publish`, { source_revision: saved.generation });
  const current = await client.beta.agents.retrieve(agent.id, { betas: BETAS });
  const updated = await client.beta.agents.update(agent.id, { name: 'Renamed finite Agent', version: current.version, betas: BETAS });
  assert.equal((await config('GET', `/agents/${agent.id}`)).max_steps, 5, 'R2 Managed update preserves private limit');
  assert.ok(!Object.hasOwn(updated, 'max_steps'), 'R2 official response stays official');
  await stopServer(server); server = undefined; server = launch(); await waitForPort(PORT, 30_000);
  const recovered = clientFor();
  const before = rows[0];
  const events = await allEvents(recovered, before.session_id);
  assert.equal(events.filter((e) => e.type === 'span.model_request_end').length, before.model_requests, 'R3 terminal recovery does not replay model work');
  await run(recovered, agent, vault, 'old-version-after-restart');
  await run(recovered, updated, vault, 'finite-after-restart', true);
  fs.writeFileSync(path.join(directory, 'step-limit-evidence.json'), JSON.stringify({ pass: true, live, model, rows }, null, 2));
}
try { await main(); }
catch (error) { console.error(`E2E FAIL: ${error.stack}`); process.exitCode = 1; }
finally {
  if (server) await stopServer(server);
  if (fixture) await fixture.close();
  if (upstream) await upstream.close();
  cleanupFixtureTree(data);
}
