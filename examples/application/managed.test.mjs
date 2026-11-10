import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { spawn } from 'node:child_process';
import { test } from 'node:test';

const processed_at = '2026-01-01T00:00:00Z';
async function fixture(run, options = {}) {
  let sends = 0, reads = 0;
  const bodies = [];
  const server = createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks)) : null;
    if (request.method === 'POST') { sends++; bodies.push(body); }
    response.setHeader('content-type', 'application/json');
    if (options.httpError) { response.writeHead(options.httpError).end(JSON.stringify({ error: { type: 'invalid_request_error', message: 'rejected fixture' } })); return; }
    if (request.url.includes('/events') && request.method === 'POST') {
      response.end(JSON.stringify({ data: options.noReceipt ? [] : [{ id: 'receipt-1', processed_at, type: 'user.message' }] })); return;
    }
    if (request.url.includes('/events')) {
      reads++;
      if (options.hangRead) return;
      const waiting = reads <= (options.pendingReads ?? 0);
      const data = [
        { id: 'old', processed_at, type: 'agent.message', content: [{ type: 'text', text: 'OLD REPLY' }] },
        { id: 'old-idle', processed_at, type: 'session.status_idle', stop_reason: { type: 'end_turn' } },
        ...(!waiting ? [{ id: 'receipt-1', processed_at, type: 'user.message' },
          ...(options.error ? [{ id: 'error', processed_at, type: 'session.error', error: { type: 'unknown_error', message: 'fixture failure' } }] : []),
          { id: 'answer', processed_at, type: 'agent.message', content: [{ type: 'text', text: 'CURRENT REPLY' }] },
          { id: 'idle', processed_at, type: options.error ? 'session.status_terminated' : 'session.status_idle', stop_reason: { type: options.reason ?? 'end_turn', event_ids: ['tool-1'] } }] : []),
      ];
      response.end(JSON.stringify({ data, has_more: false })); return;
    }
    response.end(JSON.stringify({ id: 'sesn_example', type: 'session', status: 'idle' }));
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  try {
    await run({ baseURL: `http://127.0.0.1:${server.address().port}`, stats: () => ({ sends, reads, bodies }) });
  } finally { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
}

async function execute(baseURL, args = [], env = {}) {
  const child = spawn(process.execPath, ['managed.mjs', ...args], {
    cwd: import.meta.dirname,
    env: { ...process.env, AWAKEN_BASE_URL: baseURL, AWAKEN_API_KEY: 'fixture-credential', AWAKEN_AGENT_ID: 'assistant', AWAKEN_ENVIRONMENT_ID: 'env_example', AWAKEN_TIMEOUT_MS: '3000', ...env }, // awaken-allow(secret): deterministic HTTP fixture value
  });
  let stdout = '', stderr = '';
  child.stdout.on('data', chunk => { stdout += chunk; });
  child.stderr.on('data', chunk => { stderr += chunk; });
  const [code] = await once(child, 'exit'); return { code, stdout, stderr };
}

// Cause/effect graph: C1 new/reused Session; C2 exact accepted receipt absent or
// not yet committed; C3 older idle/reply exists; C4 current idle reason is natural,
// requires_action, budget, refusal or terminal error; C5 transport rejects; C6
// observation expires. Effects: E1 success prints only the new answer; E2 waits
// preserve Session/receipt and exit 2; E3 failures exit 1; E4 no mutation retries.
// Decision rules: M1 C1+C2+C3 -> observe, never old success; M2 current natural
// -> E1; M3 action/budget -> E2; M4 refusal/error -> E3; M5 C5/C6 -> E3+E4;
// M6 retained receipt/history -> read only. HTTP fixtures prove client behavior;
// the separate production E2E proves Awaken execution with these same sources.
test('M1/M2 follows the exact new receipt, retaining one reused Session', async () => fixture(async ({ baseURL, stats }) => {
  const result = await execute(baseURL, ['--session', 'sesn_example', 'hello']);
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /CURRENT REPLY/); assert.doesNotMatch(result.stdout, /OLD REPLY/);
  assert.equal(stats().sends, 1);
  assert.ok(stats().reads >= 3);
}, { pendingReads: 2 }));

for (const reason of ['requires_action', 'budget_reached', 'refusal']) {
  test(`M3/M4 ${reason} has an explicit non-success exit`, async () => fixture(async ({ baseURL, stats }) => {
    const result = await execute(baseURL, ['--session', 'sesn_example']);
    assert.equal(result.code, reason === 'refusal' ? 1 : 2, result.stderr);
    assert.match(result.stdout, /receipt=receipt-1/); assert.equal(stats().sends, 1);
  }, { reason }));
}
test('M4 terminal errors remain errors', async () => fixture(async ({ baseURL }) => {
  const result = await execute(baseURL, ['--session', 'sesn_example']);
  assert.equal(result.code, 1); assert.match(result.stderr, /fixture failure/);
}, { error: true }));
test('M5 timeout keeps the exact receipt without resubmitting', async () => fixture(async ({ baseURL, stats }) => {
  const result = await execute(baseURL, ['--session', 'sesn_example'], { AWAKEN_TIMEOUT_MS: '250' });
  assert.equal(result.code, 1); assert.match(result.stderr, /--receipt receipt-1/);
  assert.equal(stats().sends, 1); assert.doesNotMatch(result.stdout, /OLD REPLY/);
}, { pendingReads: 100 }));
test('M5 HTTP refusal is not retried', async () => fixture(async ({ baseURL, stats }) => {
  const result = await execute(baseURL, ['--session', 'sesn_example']);
  assert.equal(result.code, 1); assert.match(result.stderr, /HTTP 401/); assert.equal(stats().sends, 0);
}, { httpError: 401 }));
test('M5 missing receipt cannot be guessed from history', async () => fixture(async ({ baseURL, stats }) => {
  const result = await execute(baseURL, ['--session', 'sesn_example']);
  assert.equal(result.code, 1); assert.match(result.stderr, /no Event receipt/); assert.equal(stats().sends, 1);
}, { noReceipt: true }));
test('M6 saved receipt is observation only', async () => fixture(async ({ baseURL, stats }) => {
  const result = await execute(baseURL, ['--session', 'sesn_example', '--receipt', 'receipt-1']);
  assert.equal(result.code, 0); assert.equal(stats().sends, 0); assert.match(result.stdout, /CURRENT REPLY/);
}));
test('M6 history and approval use their existing public operations', async () => fixture(async ({ baseURL, stats }) => {
  const history = await execute(baseURL, ['--session', 'sesn_example', '--history']);
  assert.equal(history.code, 0); assert.equal(stats().sends, 0);
  const approval = await execute(baseURL, ['--session', 'sesn_example', '--approve', 'tool-1']);
  assert.equal(approval.code, 0);
  assert.deepEqual(stats().bodies[0].events, [{ type: 'user.tool_confirmation', tool_use_id: 'tool-1', result: 'allow' }]);
}));
test('M1 first use creates one Session; invalid config causes no network work', async () => fixture(async ({ baseURL, stats }) => {
  const created = await execute(baseURL, ['hello']);
  assert.equal(created.code, 0); assert.equal(stats().sends, 2);
  const before = stats().sends;
  const invalid = await execute(baseURL, ['--approve', 'tool-1']);
  assert.equal(invalid.code, 1); assert.equal(stats().sends, before);
}));
test('M1 the package-manager separator preserves option semantics', async () => fixture(async ({ baseURL, stats }) => {
  // M7 package-manager leading -- + existing Session -> the same read-only
  // history operation, never a new Session or a message containing CLI flags.
  const result = await execute(baseURL, ['--', '--session', 'sesn_example', '--history']);
  assert.equal(result.code, 0, result.stderr); assert.equal(stats().sends, 0);
}));
test('M5 a stalled read also respects the observation deadline', async () => fixture(async ({ baseURL, stats }) => {
  // M8 accepted input + nonresponding history -> retained receipt, bounded
  // abort, no second send. The deadline owns the entire paginated read.
  const result = await execute(baseURL, ['--session', 'sesn_example'], { AWAKEN_TIMEOUT_MS: '250' });
  assert.equal(result.code, 1); assert.match(result.stderr, /--receipt receipt-1/);
  assert.equal(stats().sends, 1);
}, { hangRead: true }));
test('M1 help explains the interface without configuration or network', async () => fixture(async ({ baseURL, stats }) => {
  const result = await execute(baseURL, ['--help'], { AWAKEN_AGENT_ID: '', AWAKEN_ENVIRONMENT_ID: '' });
  assert.equal(result.code, 0); assert.match(result.stdout, /Usage:/); assert.equal(stats().sends, 0);
}));
