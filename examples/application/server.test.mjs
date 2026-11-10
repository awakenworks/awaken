import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { spawn } from 'node:child_process';

// Causes: C1 backend key vs browser token; C2 repeated token acquisition; C3
// allowed vs foreign origin/path/method; C4 upstream success vs refusal.
// Effects: E1 browser receives only short-lived capability; E2 the fixed Session
// binding never creates a new Session; E3 proxy forwards only the app credential;
// E4 refusals retain their status. Rules B1 C1+C2->E1/E2; B2 allowed proxy->E3;
// B3 foreign origin/path/method -> reject before upstream; B4 upstream refusal
// -> E4. This is a one-operator example, not a customer authorization service.
test('B1-B4 keep token issuance and protocol credentials separate', async () => {
  const calls = [];
  const upstream = createServer(async (request, response) => {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    calls.push({ url: request.url, authorization: request.headers.authorization, body: chunks.length ? JSON.parse(Buffer.concat(chunks)) : null });
    response.setHeader('content-type', 'application/json');
    if (request.url === '/v1/application-access-tokens') {
      response.end(JSON.stringify({ access_token: 'browser-capability', expires_at: '2099-01-01T00:00:00Z' })); // awaken-allow(secret): deterministic HTTP fixture value
    } else if (request.method === 'DELETE') response.writeHead(403).end('{}');
    else response.end(JSON.stringify({ items: [] }));
  });
  upstream.listen(0, '127.0.0.1'); await once(upstream, 'listening');
  const child = spawn(process.execPath, ['server.mjs'], {
    cwd: import.meta.dirname,
    env: { ...process.env, PORT: '0', AWAKEN_BASE_URL: `http://127.0.0.1:${upstream.address().port}`, AWAKEN_SESSION_ID: 'sesn_existing', AWAKEN_API_KEY: 'backend-only-credential' }, // awaken-allow(secret): deterministic HTTP fixture value
  });
  let output = ''; child.stdout.on('data', data => { output += data; });
  try {
    const deadline = Date.now() + 5000;
    while (!/http:\/\/127\.0\.0\.1:\d+/.test(output) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 20));
    const base = output.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0]; assert.ok(base, output);
    for (let index = 0; index < 2; index++) {
      const result = await fetch(`${base}/api/connection`, { method: 'POST' });
      const body = await result.json(); assert.equal(result.status, 200);
      assert.equal(body.access_token, 'browser-capability'); assert.equal(body.thread_id, 'example-chat');
      assert.doesNotMatch(JSON.stringify(body), /backend-only-credential|sesn_existing/);
      assert.equal(result.headers.get('cache-control'), 'no-store');
    }
    assert.equal(calls.length, 2);
    assert.ok(calls.every(call => call.authorization === 'Bearer backend-only-credential'));
    assert.ok(calls.every(call => call.body.thread_bindings[0].managed_session_id === 'sesn_existing'));
    const history = await fetch(`${base}/awaken/v1/ai-sdk/threads/example-chat/messages`, { headers: { Authorization: 'Bearer browser-capability' } });
    assert.equal(history.status, 200); assert.equal(calls.at(-1).authorization, 'Bearer browser-capability');
    const stop = await fetch(`${base}/awaken/v1/ai-sdk/threads/example-chat/runs`, { method: 'DELETE', headers: { Authorization: 'Bearer browser-capability' } });
    assert.equal(stop.status, 403);
    const count = calls.length;
    assert.equal((await fetch(`${base}/api/connection`, { method: 'POST', headers: { Origin: 'https://foreign.example' } })).status, 403);
    assert.equal((await fetch(`${base}/api/connection`, { method: 'POST', headers: { Host: 'foreign.example:4317', Origin: 'http://foreign.example:4317' } })).status, 403);
    assert.equal((await fetch(`${base}/awaken/v1/sessions`, { method: 'POST' })).status, 404);
    assert.equal((await fetch(`${base}/awaken/v1/ai-sdk/threads/another-thread/runs`, { method: 'POST' })).status, 404);
    assert.equal(calls.length, count);
  } finally {
    child.kill('SIGTERM'); await once(child, 'exit');
    upstream.closeAllConnections(); await new Promise(resolve => upstream.close(resolve));
  }
});
