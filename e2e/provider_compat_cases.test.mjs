import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import ts from 'typescript';
import * as driver from './provider_connection_matrix_real_e2e.mjs';
import * as trace from './trace_validate.mjs';
import { cleanupFixtureTree, deploymentEnv, runCleanupSteps, stopServer } from './harness.mjs';
import { provisionProviderTestModel } from './harness.mjs';
import {
  loadProviderCases,
  providerDialects,
  publicProviderCase,
  requiredProviderCaseIds,
  selectProviderCases,
} from './provider_compat_cases.mjs';

const authored = (cases, secrets = {}) => ({
  AWAKEN_PROVIDER_CASES_JSON: JSON.stringify(cases),
  ...secrets,
});

test('authored third-party provider is opaque, normalized, and secret-free in public evidence', () => {
  const [value] = loadProviderCases(authored([{
    id: 'vendor-v2',
    provider_id: 'vendor-v2',
    dialect: 'open_ai_chat',
    base_url: 'https://edge.vendor.example/v9',
    model_id: 'vendor/model',
    secret_env: 'VENDOR_TOKEN',
  }], { VENDOR_TOKEN: 'do-not-print' })); // awaken-allow(secret): -- inert non-leak fixture
  assert.equal(value.secret, 'do-not-print');
  assert.equal(value.timeout_secs, 300);
  assert.ok(!JSON.stringify(publicProviderCase(value)).includes('do-not-print'));
});

test('Vertex OAuth helper stays secret-free and keeps endpoint construction server-owned', () => {
  const [value] = loadProviderCases(authored([{
    id: 'vertex-prod',
    provider_id: 'vertex',
    dialect: 'vertex_gemini',
    auth: 'oauth_helper',
    oauth_helper: 'gcloud',
    configuration: { project_id: 'project-a', location: 'global' },
  }]));
  assert.equal(value.base_url, undefined);
  assert.equal(value.secret, undefined);
  assert.deepEqual(value.configuration, { project_id: 'project-a', location: 'global' });
});

test('all built-in wire dialects remain admitted', () => {
  const cases = providerDialects.map((dialect, index) => ({
    id: `case-${index}`,
    provider_id: `provider-${index}`,
    dialect,
    base_url: `https://provider-${index}.example/v1`,
    secret_env: `TOKEN_${index}`,
  }));
  const secrets = Object.fromEntries(cases.map((item) => [item.secret_env, 'secret']));
  assert.equal(loadProviderCases(authored(cases, secrets)).length, providerDialects.length);
});

test('automatic cases cover every installed provider descriptor plus a custom gateway', () => {
  // Cause/effect graph: C1 every built-in API-key provider has its credential
  // and required endpoint; C2 Vertex has its project prerequisite; C3 the
  // custom AnyRouter gateway has both credential and explicit endpoint. E1 the
  // automatic catalog emits every canonical identity exactly once, in order.
  // Constraint K1: with no authored JSON, `autoCases` is the sole catalog owner;
  // prerequisite filtering may omit a row but cannot synthesize another path.
  // Decision rule A1=C1+C2+C3=>E1. Coverage rationale: the all-enabled,
  // dependency-expanded vector detects a missing, duplicate, reordered, or
  // renamed row; adjacent normalization/rejection tests own field-level faults.
  const cases = loadProviderCases({
    ANTHROPIC_API_KEY: 'a',
    OPENAI_API_KEY: 'o',
    DEEPSEEK_API_KEY: 'd',
    KIMI_API_KEY: 'k',
    ANYROUTER_API_KEY: 'r',
    ANYROUTER_BASE_URL: 'https://router.example/v1',
    GEMINI_API_KEY: 'g',
    VERTEX_PROJECT_ID: 'project-a',
  });
  assert.deepEqual(
    cases.map(({ id }) => id),
    ['anthropic', 'openai', 'deepseek', 'kimi-anthropic', 'anyrouter', 'gemini', 'vertex'],
  );
});

test('required provider certification selects exact case identities', () => {
  // Cause/effect graph: C1 the strict lane names no/exact/duplicate/invalid
  // case ids; C2 unrelated provider credentials may also be present. E1 no
  // requirement remains opt-in; E2 exact ids are preserved in authored order;
  // E3 duplicate or unsafe ids fail before any provider I/O. Decision rules:
  // R1=!C1=>E1, R2=valid C1+any C2=>E2, R3=duplicate|invalid C1=>E3.
  // Constraints/invariant: the requested ordered identities select only the
  // already-built canonical case catalog; selection never creates another case.
  assert.deepEqual(requiredProviderCaseIds({}), [], 'R1');
  assert.deepEqual(
    requiredProviderCaseIds({ AWAKEN_PROVIDER_MATRIX_REQUIRED_CASE_IDS: 'deepseek,anthropic' }),
    ['deepseek', 'anthropic'],
    'R2',
  );
  const configured = [{ id: 'anthropic' }, { id: 'deepseek' }, { id: 'openai' }];
  assert.equal(selectProviderCases(configured, []), configured, 'R1 all configured');
  assert.deepEqual(
    selectProviderCases(configured, ['deepseek', 'anthropic']).map(({ id }) => id),
    ['deepseek', 'anthropic'],
    'R2 strict ordered subset',
  );
  assert.throws(
    () => requiredProviderCaseIds({ AWAKEN_PROVIDER_MATRIX_REQUIRED_CASE_IDS: 'deepseek,deepseek' }),
    /must be unique/u,
    'R3 duplicate',
  );
  assert.throws(
    () => requiredProviderCaseIds({ AWAKEN_PROVIDER_MATRIX_REQUIRED_CASE_IDS: '../deepseek' }),
    /invalid/u,
    'R3 unsafe',
  );
  assert.throws(
    () => selectProviderCases(configured, ['missing']),
    /not configured/u,
    'R3 missing configured case',
  );
});

for (const [name, cases, secrets, pattern] of [
  ['malformed JSON', '{', {}, /valid JSON/u],
  ['non-array JSON', '{}', {}, /must be an array/u],
  ['missing secret', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: 'https://x.example', secret_env: 'X_KEY' }], {}, /is not configured/u],
  ['relative URL', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: '/v1', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /base_url must be absolute/u],
  ['unsupported protocol', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: 'file:///tmp/x', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /must use HTTP/u],
  ['unknown dialect', [{ id: 'x', provider_id: 'x', dialect: 'mystery', base_url: 'https://x.example', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /unsupported dialect/u],
  ['invented third-party dialect', [{ id: 'x', provider_id: 'x', dialect: 'third-party/v9', base_url: 'https://x.example', secret_env: 'X_KEY' }], { X_KEY: 'x' }, /unsupported dialect/u],
  ['unsafe secret selector', [{ id: 'x', provider_id: 'x', dialect: 'open_ai_chat', base_url: 'https://x.example', secret_env: 'x-key' }], { 'x-key': 'x' }, /secret_env is invalid/u],
  ['OAuth secret ambiguity', [{ id: 'x', provider_id: 'vertex', dialect: 'vertex_gemini', auth: 'oauth_helper', oauth_helper: 'gcloud', secret_env: 'X_KEY', configuration: { project_id: 'p' } }], { X_KEY: 'x' }, /cannot select a secret/u],
  ['OAuth client-owned URL', [{ id: 'x', provider_id: 'vertex', dialect: 'vertex_gemini', auth: 'oauth_helper', oauth_helper: 'gcloud', base_url: 'https://x.example', configuration: { project_id: 'p' } }], {}, /server-owned/u],
]) {
  test(`${name} fails before provider I/O`, () => {
    const env = typeof cases === 'string'
      ? { AWAKEN_PROVIDER_CASES_JSON: cases, ...secrets }
      : authored(cases, secrets);
    assert.throws(() => loadProviderCases(env), pattern);
  });
}

test('duplicate identities fail before provider I/O', () => {
  const one = {
    id: 'duplicate',
    provider_id: 'vendor',
    dialect: 'open_ai_chat',
    base_url: 'https://vendor.example/v1',
    secret_env: 'VENDOR_KEY',
  };
  assert.throws(
    () => loadProviderCases(authored([one, one], { VENDOR_KEY: 'secret' })),
    /ids must be unique/u,
  );
});

// Setup decision graph: C1 accepted connection + nonempty discovery; C2 explicit
// model present (or omitted); C3 secret-free response; C4 profile accepted.
// P1 C1+C2+C3+C4 -> one exact-credential profile; P2 !C1 stops at connection;
// P3 !C2 stops after catalog, never substitutes another model; P4 !C3 returns
// only a redacted diagnostic; P5 !C4 fails publication setup. API-key/OAuth
// branches preserve their existing exclusive ownership of credential ingress.
// P6 cross P1-P5 with Native/ACP: Native writes an exact-credential profile;
// ACP returns an exact endpoint + runtime Target and creates no unused profile.
// A profile-only failure is unreachable for ACP. No runtime substitution occurs;
// actual capability/dialect/credential admission belongs to publication tests.
for (const backendRef of ['genai', 'acp:claude'])
for (const mode of ['key', 'oauth', 'implicit', 'missing-model', 'empty', 'leak', 'denied', 'profile-denied']) {
  test(`shared provider setup: ${backendRef} / ${mode}`, async (t) => {
    const calls = [];
    const secret = 'inert-provider-test-material'; // awaken-allow(secret): -- loopback-only test
    const item = { id: 'fixture', provider_id: 'fixture', dialect: 'open_ai_chat',
      base_url: 'https://provider.invalid', timeout_secs: 300,
      ...(mode === 'implicit' ? {} : { model_id: mode === 'missing-model' ? 'absent' : 'chosen' }),
      ...(mode === 'oauth' ? { auth: 'oauth_helper', oauth_helper: 'fixture-helper', configuration: { project_id: 'project' } }
        : { auth: 'api_key', secret }),
    };
    t.mock.method(globalThis, 'fetch', async (url, init) => {
      const route = new URL(url).pathname;
      calls.push({ route, method: init.method, body: init.body ? JSON.parse(init.body) : undefined });
      if (route === '/v1/config/provider-connections') {
        return Response.json({ endpoint: {id: 'endpoint', base_url: 'https://provider.invalid'}, credential: { id: 'credential' }, sync: { discovered: mode === 'empty' ? 0 : 2 },
          ...(mode === 'leak' ? { leaked: secret } : {}) }, { status: mode === 'denied' ? 403 : 201 });
      }
      if (route === '/v1/config/catalog') return Response.json({ offerings: [
        { model_id: 'wrong-provider', provider_id: 'other', protocol_endpoint_id: 'other' },
        { model_id: 'chosen', provider_id: 'fixture', protocol_endpoint_id: 'other-dialect', status: 'active' },
        { model_id: 'chosen', provider_id: 'fixture', protocol_endpoint_id: 'endpoint', status: 'active' },
      ] });
      assert.equal(route, '/v1/config/inference-profiles/profile');
      return Response.json({}, { status: mode === 'profile-denied' ? 400 : 200 });
    });
    if (['key', 'oauth', 'implicit'].includes(mode) || (mode === 'profile-denied' && backendRef !== 'genai')) {
      const result = await provisionProviderTestModel('http://localhost', 'workspace', item, 'profile', { backendRef });
      assert.equal(result.offering.model_id, 'chosen');
      assert.equal(calls.length, backendRef === 'genai' ? 3 : 2);
      assert.deepEqual(result.model, backendRef === 'genai' ? { mode: 'profile', profile_id: 'profile' } : 'chosen;provider=fixture;api=open_ai_chat;executor=acp:claude');
      if (backendRef === 'genai') assert.deepEqual(calls[2].body.primary, {
        target: { model_id: 'chosen', provider_id: 'fixture', protocol_endpoint_id: 'endpoint' },
        credential_binding: { type: 'exact', credential_source_id: 'credential' },
      });
      assert.equal(calls[0].body.workspace_id, 'workspace');
      assert.equal(calls[0].body.secret, mode === 'oauth' ? undefined : secret);
      assert.equal(calls[0].body.oauth_helper, mode === 'oauth' ? 'fixture-helper' : undefined);
    } else {
      await assert.rejects(provisionProviderTestModel('http://localhost', 'workspace', item, 'profile', { backendRef }), (error) => {
        assert.ok(!error.message.includes(secret));
        return true;
      });
      assert.equal(calls.length, mode === 'missing-model' ? 2 : mode === 'profile-denied' ? 3 : 1);
    }
  });
}

// P7 invalid backend coordinates must fail before credential ingress/discovery;
// a typo must not become an implicitly Native test or an arbitrary remote call.
for (const backendRef of ['', 'native', 'acp:', 'a2a:https://remote.invalid']) {
  test(`provider setup rejects invalid backend: ${backendRef}`, async (t) => {
    const fetch = t.mock.method(globalThis, 'fetch', () => { throw new Error('unexpected I/O'); });
    await assert.rejects(provisionProviderTestModel('http://localhost', 'workspace', {}, 'profile', {backendRef}),
      /requires a Native or ACP backend/u);
    assert.equal(fetch.mock.callCount(), 0);
  });
}

// P8 omitted URL delegates endpoint/discovery ownership to Control; explicit
// URLs retain syntax validation above. No provider template is copied here.
test('provider case can request the Control-owned default endpoint', () => {
  const [item] = loadProviderCases(authored([{id: 'default', provider_id: 'deepseek',
    dialect: 'anthropic_messages', secret_env: 'VENDOR_KEY'}], {VENDOR_KEY: 'inert'}));
  assert.equal(item.base_url, undefined);
});


// Provider diagnostics are observations, not a new protocol oracle. These
// official-SDK wire fixtures never start Native, a model, Cargo or a service.
// D1 good args/reply => original assertions0 + exact receipt/Thread facts.
// D2 changed send arg + any wrong public reply => SAME original reply error;
// retain both texts, never normalize or decide which layer changed them.
// D3 failure before the old list point => ONE post-failure readonly attempt;
// its unavailability is recorded and never replaces D2. D4 no raw-model port
// => explicitly unavailable. Existing model/prompt/maxAttempts3 remain owners.
const diagnosticAuthoredModel = 'profile=fixture';
const diagnosticPublications = { coordinator: { source_revision: 1 }, worker: { source_revision: 1 } };
const diagnosticMarkers = { childReply: 'AWAKEN-MANAGED-FIXTURE-CHILD-OK',
  sendAccepted: 'AWAKEN-MANAGED-FIXTURE-SEND-ACCEPTED',
  reportAck: 'AWAKEN-MANAGED-FIXTURE-REPORT-ACK' };

function providerWireFixture(t, { reply = diagnosticMarkers.childReply,
  message = `Reply with exactly ${diagnosticMarkers.childReply}`, factsUnavailable = false,
  frozenReference = diagnosticAuthoredModel, frozenVersion = 1, primaryAgent = 'coordinator' } = {}) {
  const calls = [];
  const text = value => [{ type: 'text', text: value }];
  const event = (id, type, fields = {}) => ({ id, type, processed_at: '2026-01-01T00:00:00Z', ...fields });
  const events = [
    event('accepted', 'user.message'),
    event('list', 'agent.tool_use', { name: 'list_agents', input: {} }),
    event('send', 'agent.tool_use', { name: 'send_message',
      input: { agent_id: 'worker', message, unknown_private: 'MUST-NOT-RECORD' } }),
    event('ack', 'agent.message', { content: text(diagnosticMarkers.sendAccepted) }),
    event('created', 'session.thread_created', { session_thread_id: 'child' }),
    event('running', 'session.thread_status_running', { session_thread_id: 'child' }),
    event('sent', 'agent.thread_message_sent', { to_session_thread_id: 'child', content: text(message) }),
    event('received', 'agent.thread_message_received', { from_session_thread_id: 'child', content: text(reply) }),
    event('child-idle', 'session.thread_status_idle', { session_thread_id: 'child', stop_reason: { type: 'end_turn' } }),
    event('report', 'agent.message', { content: text(diagnosticMarkers.reportAck) }),
    event('idle', 'session.status_idle', { stop_reason: { type: 'end_turn' } }),
  ];
  t.mock.method(globalThis, 'fetch', async (input, init = {}) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    const method = init.method ?? (input instanceof Request ? input.method : 'GET');
    calls.push({ path: url.pathname, method });
    if (url.pathname === '/v1/sessions' && method === 'POST') return Response.json({ id: 'session' });
    if (url.pathname === '/v1/sessions/session/events' && method === 'POST') return Response.json({ data: [events[0]] });
    if (url.pathname === '/v1/sessions/session/events') return Response.json({ data: events, has_more: false });
    if (url.pathname === '/v1/sessions/session/threads') {
      if (factsUnavailable) return Response.json({ error: { type: 'unavailable', message: 'fixture' } }, { status: 503 });
      return Response.json({ data: [
        { id: 'primary', parent_thread_id: null, agent: { id: primaryAgent, version: frozenVersion, model: { id: frozenReference } } },
        { id: 'child', parent_thread_id: 'primary', status: 'idle', agent: { id: 'worker', version: frozenVersion, model: { id: frozenReference } } },
      ], has_more: false });
    }
    if (url.pathname === '/v1/sessions/session/threads/child/events') {
      if (factsUnavailable) return Response.json({ error: { type: 'unavailable', message: 'fixture' } }, { status: 503 });
      return Response.json({ data: [event('input', 'user.message', { content: text(message) }),
        event('output', 'agent.message', { content: text(reply) })], has_more: false });
    }
    if (url.pathname === '/v1/sessions/session' && method === 'DELETE') return Response.json({ id: 'session', type: 'session.deleted' });
    if (url.pathname === '/v1/sessions/session') return Response.json({ id: 'session', status: 'idle' });
    throw new Error(`unexpected controlled SDK route ${method} ${url.pathname}`);
  });
  return { calls, events };
}

test('provider diagnostic retains receipt and real SDK facts without changing the healthy oracle', async t => {
  const f = providerWireFixture(t); const observation = { boundary: 'create' };
  await driver.exerciseManagedMultiagentAttempt('http://controlled.invalid', 'coordinator', 'worker', diagnosticMarkers, observation, diagnosticAuthoredModel, diagnosticPublications);
  assert.ok(observation.accepted_receipt, 'D1 diagnostic receipt was retained');
  assert.equal(observation.accepted_receipt.id, 'accepted', 'D1 original accepted coordinate');
  assert.equal(observation.session_id, 'session');
  assert.equal(observation.status, 'passed');
  assert.deepEqual(observation.events_after_receipt.find(e => e.id === 'send').input,
    { agent_id: 'worker', message: `Reply with exactly ${diagnosticMarkers.childReply}` });
  assert.equal(observation.frozen_threads.after_failure, false, 'D1 existing positive read point');
  assert.equal(observation.frozen_threads.items[1].agent.model_id, diagnosticAuthoredModel);
  assert.equal(observation.child_events.events[0].content[0].text, `Reply with exactly ${diagnosticMarkers.childReply}`);
  assert.equal(observation.child_events.after_failure, false);
  assert.equal(observation.raw_model_output.available, false, 'D4 SDK events are not raw provider output');
  assert.equal(observation.session_delete.status, 'completed');
  assert.ok(!JSON.stringify(observation).includes('MUST-NOT-RECORD'));
  assert.equal(f.calls.filter(c => c.path.endsWith('/threads')).length, 1);
});

for (const reply of ['AWAKEN-MI\u200b\u200bNDED-MANAGED-FIXTURE-CHILD-OK',
  'AWAKEN-M<refusal>MANAGED-FIXTURE-CHILD-OK', 'AWAKEN-MANAGED-F#IXTURE-CHILD-OK']) {
  test(`provider diagnostic preserves original reply rejection and input bytes: ${JSON.stringify(reply)}`, async t => {
    const message = `Reply with exactly ${reply}`;
    const f = providerWireFixture(t, { reply, message }); const observation = { boundary: 'create' };
    await assert.rejects(driver.exerciseManagedMultiagentAttempt('http://controlled.invalid', 'coordinator', 'worker', diagnosticMarkers, observation, diagnosticAuthoredModel, diagnosticPublications),
      error => error.code === 'ERR_ASSERTION' && error.actual === reply && error.expected === diagnosticMarkers.childReply);
    assert.ok(observation.failure, 'D2 diagnostic failure boundary was retained');
    assert.equal(observation.failure.boundary, 'exact_child_reply', 'D2 original failure stays authoritative');
    assert.equal(observation.events_after_receipt.find(e => e.id === 'send').input.message, message);
    assert.equal(observation.events_after_receipt.find(e => e.id === 'received').content[0].text, reply);
    assert.equal(observation.frozen_threads.after_failure, true, 'D3 observed later, not a pre-failure snapshot');
    assert.equal(observation.child_events.after_failure, true);
    assert.equal(observation.child_events.events[1].content[0].text, reply);
    assert.equal(observation.session_delete.status, 'completed');
    assert.equal(f.calls.filter(c => c.path.endsWith('/threads')).length, 1);
  });
}

test('provider diagnostic missing readonly facts cannot replace the primary assertion or retry the SDK read', async t => {
  const f = providerWireFixture(t, { reply: 'wrong', factsUnavailable: true }); const observation = { boundary: 'create' };
  await assert.rejects(driver.exerciseManagedMultiagentAttempt('http://controlled.invalid', 'coordinator', 'worker', diagnosticMarkers, observation, diagnosticAuthoredModel, diagnosticPublications),
    error => error.code === 'ERR_ASSERTION' && error.actual === 'wrong');
  assert.equal(observation.frozen_threads.available, false);
  assert.equal(observation.child_events.available, false);
  assert.equal(observation.session_delete.status, 'completed');
  assert.equal(f.calls.filter(c => c.path.endsWith('/threads')).length, 1, 'D3 diagnostic maxRetries0');
  assert.equal(f.calls.filter(c => c.path.endsWith('/child/events')).length, 1);
});

test('provider diagnostic keeps original three attempts and all three rejected public replies', async t => {
  const f = providerWireFixture(t, { reply: 'wrong' }); const observations = [];
  await assert.rejects(driver.managedMultiagentRun('http://controlled.invalid', 'coordinator', 'worker', diagnosticMarkers, observations, diagnosticAuthoredModel, diagnosticPublications),
    /attempt 1: exact real child reply[\s\S]*attempt 2:[\s\S]*attempt 3:/u);
  assert.equal(observations.length, 3);
  assert.deepEqual(observations.map(o => o.status), ['failed', 'failed', 'failed']);
  assert.equal(f.calls.filter(c => c.path === '/v1/sessions' && c.method === 'POST').length, 3);
});

// D5 unknown fields/content never become a credential/header/env dump. The
// projection records existing SDK data; it neither reparses nor normalizes it.
test('provider diagnostic public projection excludes unknown material and preserves exact text', () => {
  const token = 'NOT-A-REAL-CREDENTIAL'; // awaken-allow(secret): inert exclusion-test sentinel, never a credential input
  const observed = driver.publicAttemptEvent({ id: 'call', type: 'agent.tool_use', name: 'send_message',
    input: { agent_id: 'worker', message: 'M\u200b<refusal>#', secret: token, headers: { authorization: token } },
    secret: token, env: { KEY: token }, content: [{ type: 'text', text: 'M\u200b<refusal>#', secret: token }] });
  assert.equal(observed.input.message, 'M\u200b<refusal>#');
  assert.equal(observed.content[0].text, 'M\u200b<refusal>#');
  assert.ok(!JSON.stringify(observed).includes(token));
});

// Reuse the repository's TypeScript AST mechanism to execute the ACTUAL private
// certifyProvider body. Only construction/model/setup IO leaves are declared;
// the real existing stopServer owns a real Node child's EOF/exit/close. This is
// controlled wiring evidence, not a provider, Native process or Kernel test.
const providerSource = fs.readFileSync(new URL('./provider_connection_matrix_real_e2e.mjs', import.meta.url), 'utf8');
const providerAst = ts.createSourceFile('provider_connection_matrix_real_e2e.mjs', providerSource, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
const certifyDeclaration = providerAst.statements.find(node => ts.isFunctionDeclaration(node) && node.name?.text === 'certifyProvider');
assert.ok(certifyDeclaration);

for (const exitCode of [0, 37])
for (const primaryFailure of [false, true]) {
  test(`provider diagnostic strict cleanup preserves outcomes; exit=${exitCode}; primary=${primaryFailure}`, async t => {
    // D6 exit0 closes/removes; primary+exit0 still rejects the original error.
    // functional0+exit37 => reject/retain; functional-error+exit37 => keep
    // SAME primary error and independently retain cleanup failure. No fallback
    // no-op stopper or force signal is introduced by the diagnostic caller.
    const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'provider-cleanup-law-'));
    let child; let ownedDirectory;
    t.after(async () => {
      if (child) await stopServer(child);
      cleanupFixtureTree(temp);
    });
    const primary = Object.assign(new Error('controlled original functional failure'), { code: 'ORIGINAL' });
    const context = vm.createContext({ assert, fs, path, os: { tmpdir: () => temp }, performance,
      BASE_PORT: 1, process: { env: {} }, publicProviderCase,
      deploymentEnv,
      spawnServer: (_mode, _port, env) => {
        ownedDirectory = path.dirname(env.HOME);
        fs.writeFileSync(path.join(ownedDirectory, 'platform-workspace-id'), 'workspace');
        child = strictStopNodeChild(exitCode);
        return { server: child };
      },
      waitForPort: async () => { await once(child.stdout, 'data'); },
      provisionProviderTestModel: async () => ({ offering: { model_id: 'selected' }, discoveredModels: 1, model: 'selected' }),
      publishManagementAgent: async () => ({ installed: true }),
      managedRun: async () => { if (primaryFailure) throw primary; },
      pass: () => {}, stopServer, cleanupFixtureTree, runCleanupSteps, assertThreadModelRequests: trace.assertThreadModelRequests,
      readSpans: () => { throw new Error('no Native trace in controlled leaf'); },
    });
    const actual = new vm.Script(`${certifyDeclaration.getText(providerAst)}\ncertifyProvider;`).runInContext(context);
    const observation = {};
    if (primaryFailure || exitCode !== 0) {
      await assert.rejects(actual({ id: 'fixture' }, 0, observation), error => {
        assert.ok(error instanceof AggregateError, 'D6 existing runCleanupSteps owns aggregation');
        if (primaryFailure) assert.equal(error.errors[0], primary, 'D6 preserve the primary object');
        assert.equal(error.errors.some(item => /failed clean shutdown/u.test(item.message)), exitCode !== 0);
        return true;
      });
    } else {
      assert.equal(await actual({ id: 'fixture' }, 0, observation), observation);
    }
    assert.equal(child.exitCode, exitCode);
    assert.equal(observation.status, primaryFailure || exitCode !== 0 ? 'failed' : 'passed');
    assert.equal(observation.cleanup.server.status, exitCode === 0 ? 'passed' : 'failed');
    assert.equal(observation.cleanup.directory.status, exitCode === 0 ? 'removed' : 'retained');
    assert.equal(fs.existsSync(ownedDirectory), exitCode !== 0);
  });
}


// D8 a healthy reply reaches the ORIGINAL positive Threads.list, which returns
// 503 after its unchanged default two transport retries. Effects: preserve that
// SDK503 as primary, record this read as unavailable/after_failure=false, and
// do not perform another post-failure Threads.list. The new child-event read
// remains separately best-effort and Session deletion still occurs.
test('provider diagnostic original readonly failure is explicit and never retried as a second read', async t => {
  const f = providerWireFixture(t, { factsUnavailable: true });
  const observation = { boundary: 'create' };
  await assert.rejects(driver.exerciseManagedMultiagentAttempt('http://controlled.invalid', 'coordinator', 'worker', diagnosticMarkers, observation, diagnosticAuthoredModel, diagnosticPublications),
    error => error.status === 503);
  assert.ok(observation.frozen_threads, 'D8 failed original read retained explicit unavailable facts');
  assert.equal(observation.frozen_threads.available, false);
  assert.equal(observation.frozen_threads.after_failure, false);
  assert.equal(typeof observation.frozen_threads.observed_at, 'string');
  assert.equal(observation.failure.boundary, 'frozen_model_assertions');
  assert.equal(f.calls.filter(c => c.path.endsWith('/threads')).length, 3, 'D8 original one SDK call + its default two retries only');
  assert.equal(observation.session_delete.status, 'completed');
});


// Two-layer E7 table. The authored reference/publication revisions are fixture
// INPUT expectations, not derived from frozen reads. The execution expectation
// comes independently from the selected offering. E1 both right =>0;
// E2 only profile/version/primary identity wrong => reject selection despite
// valid request model. Same-profile/same-version primaryOther must not pass.
for (const changed of [{ frozenReference: 'profile=other' }, { frozenVersion: 2 }, { primaryAgent: 'other' }]) {
  test(`provider E7 frozen authored identity remains independent: ${JSON.stringify(changed)}`, async t => {
    providerWireFixture(t, changed); const observation = { boundary: 'create' };
    await assert.rejects(driver.exerciseManagedMultiagentAttempt('http://controlled.invalid', 'coordinator', 'worker', diagnosticMarkers, observation,
      diagnosticAuthoredModel, diagnosticPublications), error => error.code === 'ERR_ASSERTION');
    assert.equal(observation.failure.boundary, 'frozen_model_assertions');
    assert.doesNotThrow(() => trace.assertThreadModelRequests(requestTrace(), 'selected', { coordinator: 'session', child: 'child' }));
  });
}

function requestTrace() {
  const tid = '1'.repeat(32); const sid = value => String(value).padStart(16, '0');
  const span = (id, name, parent, attributes) => ({ name, trace_id: tid, span_id: sid(id), parent_span_id: parent === null ? null : sid(parent), attributes });
  return [span(1, 'http.request', null, { 'http.route': '/v1/sessions/{id}/events' }),
    span(2, 'invoke_agent', 1, { 'gen_ai.operation.name': 'invoke_agent', 'awaken.thread.id': 'session', 'awaken.run.id': 'coordinator-run' }),
    span(3, 'chat', 2, { 'gen_ai.operation.name': 'chat', 'gen_ai.request.model': 'selected' }),
    span(4, 'invoke_agent', 1, { 'gen_ai.operation.name': 'invoke_agent', 'awaken.thread.id': 'child', 'awaken.run.id': 'child-run' }),
    span(5, 'chat', 4, { 'gen_ai.operation.name': 'chat', 'gen_ai.request.model': 'selected' })];
}

test('provider E7 request attribution joins existing ancestry and both independent role targets', () => {
  const observed = trace.assertThreadModelRequests(requestTrace(), 'selected', { coordinator: 'session', child: 'child' });
  assert.equal(observed.roles.coordinator.length, 1);
  assert.equal(observed.roles.child.length, 1);
});

// E3 wrong coordinator/child target => reject despite a correct profile.
// E4 missing role/parent/model or changed thread/trace => unproven/reject;
// never substitute the frozen profile/current catalog for missing request facts.
for (const cause of ['coordinator-model', 'child-model', 'missing-child', 'missing-parent', 'missing-model', 'wrong-thread', 'wrong-trace']) {
  test(`provider E7 request facts fail closed: ${cause}`, () => {
    const spans = requestTrace();
    if (cause === 'coordinator-model') spans[2].attributes['gen_ai.request.model'] = 'other';
    if (cause === 'child-model') spans[4].attributes['gen_ai.request.model'] = 'other';
    if (cause === 'missing-child') spans.pop();
    if (cause === 'missing-parent') spans[4].parent_span_id = null;
    if (cause === 'missing-model') delete spans[4].attributes['gen_ai.request.model'];
    if (cause === 'wrong-thread') spans[3].attributes['awaken.thread.id'] = 'foreign';
    if (cause === 'wrong-trace') spans[4].trace_id = '2'.repeat(32);
    assert.throws(() => trace.assertThreadModelRequests(spans, 'selected', { coordinator: 'session', child: 'child' }));
  });
}

// E5 integration of existing caller + existing aggregate/strict stopper only:
// protocol0,targetwrong => overall1/cleanup0; protocolwrong,targetok => original
// primary remains errors[0], attribution0/cleanup0; stop1 => retain/unproven.
// E6 protocol0 with empty attempts => attribution failure, never empty-map0.
// E7 requested mode fails before attempts => not_reached, preserve primary.
// The protocol/setup leaves declare data, not a fake Session/Run authority.
for (const cause of ['wrong-target', 'protocol-error', 'stop-error', 'missing-role', 'missing-parent', 'missing-model', 'missing-attempt', 'not-reached']) {
  test(`provider E7 wiring keeps functional attribution separate from cleanup: ${cause}`, async t => {
    const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'provider-E7-wiring-'));
    let child; let ownedDirectory;
    t.after(async () => {
      if (child) await stopServer(child);
      cleanupFixtureTree(temp);
    });
    const primary = Object.assign(new Error('original exact child reply'), { code: 'ERR_ASSERTION' });
    const spans = requestTrace();
    if (cause === 'wrong-target') spans[4].attributes['gen_ai.request.model'] = 'other';
    if (cause === 'missing-role') spans.pop();
    if (cause === 'missing-parent') spans[4].parent_span_id = null;
    if (cause === 'missing-model') delete spans[4].attributes['gen_ai.request.model'];
    const exitCode = cause === 'stop-error' ? 37 : 0;
    const context = vm.createContext({ assert, fs, path, os: { tmpdir: () => temp }, performance,
      BASE_PORT: 1, process: { env: { AWAKEN_PROVIDER_MATRIX_MANAGED_MULTIAGENT: '1' } }, publicProviderCase,
      deploymentEnv,
      spawnServer: (_mode, _port, env) => {
        ownedDirectory = path.dirname(env.HOME); fs.writeFileSync(path.join(ownedDirectory, 'platform-workspace-id'), 'workspace');
        child = strictStopNodeChild(exitCode);
        return { server: child };
      },
      waitForPort: async () => { await once(child.stdout, 'data'); },
      provisionProviderTestModel: async () => ({ offering: { model_id: 'selected' }, discoveredModels: 1,
        model: { mode: 'profile', profile_id: 'provider-fixture' } }),
      publishManagementAgent: async () => ({ installed: true, source_revision: 1 }),
      managedRun: async () => { if (cause === 'not-reached') throw primary; },
      managedMultiagentRun: async (...args) => {
        // The same controlled IO leaf accepts the old/private call shape for
        // the old-red comparison; no production compatibility path is added.
        const observations = args.find(Array.isArray);
        if (cause !== 'missing-attempt') observations.push({ attempt: 1, session_id: 'session', child_thread_id: 'child' });
        if (cause === 'protocol-error') throw primary;
      },
      pass: () => {}, stopServer, cleanupFixtureTree, runCleanupSteps,
      readSpans: () => spans, assertThreadModelRequests: trace.assertThreadModelRequests,
    });
    const actual = new vm.Script(`${certifyDeclaration.getText(providerAst)}\ncertifyProvider;`).runInContext(context);
    const observation = {};
    await assert.rejects(actual({ id: 'fixture' }, 0, observation), error => {
      if (cause === 'protocol-error' || cause === 'not-reached') assert.equal(error.errors[0], primary, 'E5 preserve the original primary');
      return true;
    });
    assert.ok(observation.attribution, 'E5 attribution status is independently retained');
    assert.equal(observation.attribution.status, cause === 'protocol-error' ? 'passed' : cause === 'stop-error' ? 'unproven' : cause === 'not-reached' ? 'not_reached' : 'failed');
    assert.equal(observation.status, 'failed');
    assert.equal(observation.cleanup.server.status, exitCode === 0 ? 'passed' : 'failed');
    assert.equal(observation.cleanup.directory.status, exitCode === 0 ? 'removed' : 'retained');
    assert.equal(fs.existsSync(ownedDirectory), exitCode !== 0);
  });
}

// One test-only Node EOF leaf shared by the strict-cleanup and E7-wiring tables.
function strictStopNodeChild(exitCode) {
  return spawn(process.execPath, ['-e', `process.stdin.resume();process.stdin.on('end',()=>process.exit(${exitCode}));process.stdout.write('ready\\n')`],
    { stdio: ['pipe', 'pipe', 'pipe'] });
}
