import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import { TEMPLATE_IDS, templateScenario } from './template_scenarios.mjs';
import { startTemplateMcp } from './template_mcp_fixture.mjs';
import { assertTemplateResult } from './template_scenario_assertions.mjs';
import { provisionProviderTestModel } from '../harness.mjs';

test('nine templates have inputs, bounded tools and output contracts', () => {
  // T1 all nine jobs, with/without Files/MCP and optional schedules, have an
  // artifact oracle; T2 unknown ids fail. A missing case can never be a skip.
  assert.equal(TEMPLATE_IDS.length, 9);
  assert.equal(new Set(TEMPLATE_IDS).size, 9);
  for (const id of TEMPLATE_IDS) {
    const row = templateScenario(id, 'unit');
    assert.ok(row.artifacts.includes('result.json'));
    assert.ok(Object.keys(row.files).length || Object.keys(row.records).length);
    assert.ok(!row.task.includes(row.version), 'source version is not given to the model in its task');
  }
  assert.throws(() => templateScenario('unknown', 'unit'));
});

test('invoice oracle rejects wrong arithmetic, invented missing fields and extra keys', () => {
  // I1 correct schema/data->pass; I2 wrong sum, I3 invented PO, I4 extra key,
  // I5 stale source->reject. Output mutations are independent of model prose.
  const scenario = templateScenario('structured-extractor', 'unit');
  const expected = { source_version: scenario.version, invoice_id: 'INV-204', issued_at: '2026-09-03', currency: 'SGD', subtotal: 53.5, purchase_order: null, line_items: [{ quantity: 3, unit_price: 12.5 }, { quantity: 2, unit_price: 8 }] };
  const verify = (value) => assertTemplateResult(scenario, { calls: [], effects: [] }, { 'result.json': Buffer.from(JSON.stringify(value)) });
  verify(expected);
  for (const patch of [{ subtotal: 54 }, { purchase_order: 'invented' }, { unsupported: true }, { source_version: 'stale' }]) assert.throws(() => verify({ ...expected, ...patch }));
});

test('research oracle requires source reads, exact citations and bounded inference', () => {
  // R1 two reads+correct citations/contradiction->pass; R2 no reads, R3 invented
  // URL, R4 unwarranted production recommendation->reject.
  const s = templateScenario('deep-research', 'unit');
  const result = { source_version: s.version, latencies_ms: [180, 260], production_evidence: false, recommendation: 'pilot', source_urls: s.records.research.map((r) => r.url) };
  const state = { effects: [], calls: s.records.research.map((r) => ({ connector: 'research', name: 'read_source', input: { id: r.id } })) };
  const verify = (r, st = state) => assertTemplateResult(s, st, { 'result.json': Buffer.from(JSON.stringify(r)), 'brief.md': Buffer.from(`180 260 ${result.source_urls.join(' ')}`) });
  verify(result);
  assert.throws(() => verify(result, { ...state, calls: [] }));
  assert.throws(() => verify({ ...result, source_urls: ['https://invented.invalid'] }));
  assert.throws(() => verify({ ...result, recommendation: 'deploy' }));
});

test('contract oracle rejects ambiguous receipt shape, wrong dates and partial quotes', () => {
  // C1 two actual receipts+correct calendar dates+full clauses->pass;
  // C2 receipt map instead of the specified array, C3 wrong date, C4 a
  // fragment masquerading as a quote -> reject without normalizing evidence.
  const s = templateScenario('contract-clause-extraction', 'unit');
  const result = { source_version: s.version, payment_due: '2026-10-15', renewal_notice_due: '2026-12-01', ambiguous_obligations: ['training date unspecified'], task_ids: ['payment-id', 'renewal-id'] };
  const state = { calls: [{ connector: 'box', name: 'read_file' }], effects: [
    { connector: 'asana', id: 'payment-id', source_id: 'contract-17', due_date: '2026-10-15', body: 'Payment of SGD 1200 is due on 2026-10-15.' },
    { connector: 'asana', id: 'renewal-id', source_id: 'contract-17', due_date: '2026-12-01', body: 'Renewal notice must be received 30 calendar days before expiry on 2026-12-31.' },
  ] };
  const verify = (r, st = state) => assertTemplateResult(s, st, { 'result.json': Buffer.from(JSON.stringify(r)) });
  verify(result);
  assert.throws(() => verify({ ...result, task_ids: { payment: 'payment-id', renewal: 'renewal-id' } }), /expected an array/);
  assert.throws(() => verify({ ...result, renewal_notice_due: '2026-12-02' }));
  assert.throws(() => verify(result, { ...state, effects: state.effects.map((e) => ({ ...e, body: 'Cedar' })) }));
});

test('escalation oracle distinguishes exception class from message and requires linked receipts', () => {
  // E1 observed repro + class-only field + three linked effects -> pass;
  // E2 full message or wrong class, E3 unlinked note -> reject. The prompt
  // names the class-only contract; the oracle is never relaxed to accept prose.
  const scenario = templateScenario('support-to-eng-escalator', 'unit');
  const state = { calls: [{ connector: 'intercom', name: 'get_conversation' }], effects: [
    { connector: 'atlassian', id: 'ticket', source_id: 'CONV-7', url: 'https://fixture.test/ticket', body: 'Observed TypeError in repro.py' },
    { connector: 'intercom', id: 'note', body: 'https://fixture.test/ticket' },
    { connector: 'slack', id: 'status', body: 'https://fixture.test/ticket' },
  ] };
  const result = { source_version: scenario.version, reproduced: true, error_type: 'TypeError', ticket_id: 'ticket', note_id: 'note', status_id: 'status' };
  const verify = (value, evidence = state) => assertTemplateResult(scenario, evidence, { 'result.json': Buffer.from(JSON.stringify(value)), 'repro.log': Buffer.from("TypeError: 'NoneType' object is not iterable") });
  verify(result);
  for (const error_type of ["TypeError: 'NoneType' object is not iterable", 'ValueError']) assert.throws(() => verify({ ...result, error_type }));
  assert.throws(() => verify(result, { ...state, effects: state.effects.map((e) => e.connector === 'intercom' ? { ...e, body: 'unlinked' } : e) }));
});

async function connected(fixture, name, token = fixture.token) {
  const client = new Client({ name: 'template-fixture-test', version: '1.0.0' });
  try {
    await client.connect(new StreamableHTTPClientTransport(new URL(fixture.servers.find((s) => s.name === name).url), { requestInit: { headers: { authorization: `Bearer ${token}` } } }));
    return client;
  } catch (error) { await client.close(); throw error; }
}

test('MCP enforces auth and owns idempotent business write receipts', async () => {
  // M1 wrong credential->no effect; M2 read->source-only evidence;
  // M3 same key+payload->same receipt/one effect; M4 same key+changed payload
  // ->conflict/no new effect. The official SDK owns MCP framing and transport.
  const s = templateScenario('support-agent', 'unit'), f = await startTemplateMcp(s);
  let notion, slack;
  try {
    await assert.rejects(connected(f, 'notion', 'wrong'));
    assert.equal(f.state.effects.length, 0);
    notion = await connected(f, 'notion');
    assert.deepEqual((await notion.listTools()).tools.map((t) => t.name).sort(), ['read_page', 'search']);
    const read = await notion.callTool({ name: 'read_page', arguments: { id: 'support-policy' } });
    assert.equal(JSON.parse(read.content[0].text).verification_token, s.version);
    slack = await connected(f, 'slack');
    const args = { external_id: 'same', title: 'Handoff', body: 'No entitlement evidence.' };
    const first = await slack.callTool({ name: 'post_message', arguments: args });
    assert.deepEqual(await slack.callTool({ name: 'post_message', arguments: args }), first);
    const conflict = await slack.callTool({ name: 'post_message', arguments: { ...args, body: 'changed' } });
    assert.equal(conflict.isError, true);
    assert.equal(f.state.effects.length, 1); assert.equal(f.state.replays, 1);
  } finally { await notion?.close(); await slack?.close(); await f.close(); }
});

test('lost MCP response retains one effect for exact replay', async () => {
  // L1 remote commit then lost reply->client error; L2 same-key retry->same id,
  // one effect. This is the fixture's explicit idempotency contract, not a
  // claim that all external SaaS writes are safe to repeat.
  const f = await startTemplateMcp(templateScenario('support-agent', 'unit'), { loseResponseAfterWrite: true });
  let c;
  try {
    c = await connected(f, 'slack');
    const input = { name: 'post_message', arguments: { external_id: 'lost', title: 'Handoff', body: 'Evidence' } };
    await assert.rejects(c.callTool(input, undefined, { timeout: 3000 }));
    assert.equal(f.state.effects.length, 1);
    const retry = await c.callTool(input);
    assert.equal(JSON.parse(retry.content[0].text).id, f.state.effects[0].id);
    assert.equal(f.state.effects.length, 1);
  } finally { await c?.close(); await f.close(); }
});

test('provider setup does not create an unused profile for a Managed Agent', async (t) => {
  // P1 no profile id->public model id/no profile mutation; P2 explicit profile
  // ->existing exact profile path. One selected provider offering is reused.
  const calls = [];
  t.mock.method(globalThis, 'fetch', async (url, init) => {
    calls.push({ url, init });
    const body = url.endsWith('/provider-connections')
      ? { sync: { discovered: 1 }, endpoint: { id: 'ep' }, credential: { id: 'cred' } }
      : url.endsWith('/catalog') ? { offerings: [{ model_id: 'model', provider_id: 'deepseek', protocol_endpoint_id: 'ep' }] } : {};
    return new Response(JSON.stringify(body), { status: 200 });
  });
  const input = { id: 'unit', provider_id: 'deepseek', dialect: 'open_ai_chat', secret: 'test-only', model_id: 'model' }; // awaken-allow(secret): (synthetic fixture)
  const direct = await provisionProviderTestModel('http://fixture', 'ws', input);
  assert.equal(direct.model, 'model'); assert.equal(calls.length, 2);
  const profiled = await provisionProviderTestModel('http://fixture', 'ws', input, 'profile');
  assert.deepEqual(profiled.model, { mode: 'profile', profile_id: 'profile' });
  assert.equal(calls.filter((c) => c.url.includes('/inference-profiles/')).length, 1);
});
