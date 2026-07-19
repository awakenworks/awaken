// Streamable HTTP MCP e2e over the real compatibility adapter binary.
//
// Covers HTTP preflight, auth/session lifecycle, notification 202, protocol
// version validation, JSON and SSE final envelopes, ordered progress, and the
// standing tools/list_changed stream from one real axum process.

import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { stopServer, waitForPort } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38815);
const BASE = `http://127.0.0.1:${PORT}/mcp`;
const TOKEN = 'mcp-ts-demo-token'; // awaken-allow: secret
const VERSION = '2025-11-25';

function buildDemo(): string {
  const output = execFileSync(
    'cargo',
    [
      'build',
      '--quiet',
      '--message-format=json',
      '-p',
      'awaken-protocol-mcp',
      '--bin',
      'awaken-mcp-stdio-demo',
    ],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken-mcp-stdio-demo') {
        return message.executable;
      }
    } catch {
      // Ignore non-artifact diagnostics.
    }
  }
  throw new Error('could not resolve awaken-mcp-stdio-demo');
}

function headers(extra: Record<string, string> = {}): Record<string, string> {
  return {
    authorization: `Bearer ${TOKEN}`,
    accept: 'application/json, text/event-stream',
    'content-type': 'application/json',
    ...extra,
  };
}

async function rpc(
  message: unknown,
  extraHeaders: Record<string, string> = {},
): Promise<Response> {
  return fetch(BASE, {
    method: 'POST',
    headers: headers(extraHeaders),
    body: JSON.stringify(message),
  });
}

function sseMessages(text: string): any[] {
  return text
    .split('\n')
    .filter((line) => line.startsWith('data:'))
    .map((line) => JSON.parse(line.slice(5).trim()));
}

async function firstSseMessage(response: Response): Promise<any> {
  assert.ok(response.body, 'standing SSE response has a body');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) throw new Error('standing SSE stream ended before list_changed');
      buffer += decoder.decode(value, { stream: true });
      const line = buffer.split('\n').find((candidate) => candidate.startsWith('data:'));
      if (line) return JSON.parse(line.slice(5).trim());
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
}

async function main(): Promise<void> {
  const child = spawn(buildDemo(), [], {
    cwd: ROOT,
    env: {
      ...process.env,
      AWAKEN_MCP_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_MCP_BEARER_TOKEN: TOKEN,
      AWAKEN_MCP_DEMO_LIST_CHANGED_MS: '2500',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => (stderr += chunk.toString()));
  try {
    await waitForPort(PORT);

    const unauthenticated = await fetch(BASE, {
      method: 'POST',
      headers: { accept: 'application/json', 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 0, method: 'ping', params: {} }),
    });
    assert.equal(unauthenticated.status, 401);
    assert.match(unauthenticated.headers.get('www-authenticate') ?? '', /^Bearer/);

    const unacceptable = await fetch(BASE, {
      method: 'POST',
      headers: headers({ accept: 'text/plain' }),
      body: JSON.stringify({ jsonrpc: '2.0', id: 0, method: 'ping', params: {} }),
    });
    assert.equal(unacceptable.status, 406, 'unsupported POST Accept is rejected');
    const getWithoutSse = await fetch(BASE, { headers: headers({ accept: 'application/json' }) });
    assert.equal(getWithoutSse.status, 405, 'GET without text/event-stream is rejected');

    const initialized = await rpc({
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: {
        protocolVersion: VERSION,
        capabilities: {},
        clientInfo: { name: 'awaken-http-ts-e2e', version: '1' },
      },
    });
    assert.equal(initialized.status, 200);
    assert.equal(initialized.headers.get('mcp-protocol-version'), VERSION);
    const session = initialized.headers.get('mcp-session-id');
    assert.ok(session, 'initialize opened an HTTP session');
    assert.equal((await initialized.json() as any).result?.protocolVersion, VERSION);

    // Attach the standing GET before the demo source bumps its monotone version.
    const standing = await fetch(BASE, {
      headers: headers({
        accept: 'text/event-stream',
        'mcp-session-id': session!,
        'mcp-protocol-version': VERSION,
      }),
    });
    assert.equal(standing.status, 200);
    assert.match(standing.headers.get('content-type') ?? '', /^text\/event-stream/);
    const listChanged = firstSseMessage(standing);

    const notification = await rpc(
      { jsonrpc: '2.0', method: 'notifications/initialized', params: {} },
      { 'mcp-session-id': session!, 'mcp-protocol-version': VERSION },
    );
    assert.equal(notification.status, 202);
    assert.equal(await notification.text(), '', 'notification has no JSON-RPC response body');

    const unsupported = await rpc(
      { jsonrpc: '2.0', id: 2, method: 'ping', params: {} },
      { 'mcp-session-id': session!, 'mcp-protocol-version': '1900-01-01' },
    );
    assert.equal(unsupported.status, 200);
    assert.equal((await unsupported.json() as any).error?.code, -32602);

    const progressResponse = await rpc(
      {
        jsonrpc: '2.0',
        id: 3,
        method: 'tools/call',
        params: {
          name: 'count',
          arguments: { steps: 4 },
          _meta: { progressToken: 'http-progress' }, // awaken-allow: secret
        },
      },
      {
        accept: 'text/event-stream',
        'mcp-session-id': session!,
        'mcp-protocol-version': VERSION,
      },
    );
    assert.equal(progressResponse.status, 200);
    assert.match(progressResponse.headers.get('content-type') ?? '', /^text\/event-stream/);
    const events = sseMessages(await progressResponse.text());
    const progress = events.filter((event) => event.method === 'notifications/progress');
    assert.deepEqual(progress.map((event) => event.params.progress), [1, 2, 3, 4]);
    assert.ok(progress.every((event) => event.params.progressToken === 'http-progress'));
    assert.equal(events.filter((event) => event.id === 3).length, 1, 'one final response envelope');
    assert.equal(events.at(-1)?.result?.content?.[0]?.text, 'counted to 4', 'final response is last');

    const changed = await Promise.race([
      listChanged,
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error('timed out waiting for tools/list_changed')), 5_000),
      ),
    ]);
    assert.equal(changed.method, 'notifications/tools/list_changed');

    const deleted = await fetch(BASE, {
      method: 'DELETE',
      headers: headers({ 'mcp-session-id': session!, 'mcp-protocol-version': VERSION }),
    });
    assert.equal(deleted.status, 204);
    const staleSession = await rpc(
      { jsonrpc: '2.0', id: 4, method: 'ping', params: {} },
      { 'mcp-session-id': session!, 'mcp-protocol-version': VERSION },
    );
    assert.equal(staleSession.status, 404, 'deleted session cannot be reused');

    console.log(
      'MCP STREAMABLE HTTP TS E2E PASS: auth/preflight, session, 202 notification, version gate, JSON/SSE envelopes, progress ordering and list_changed.',
    );
  } finally {
    await stopServer(child).catch(() => {});
    if (child.exitCode && child.exitCode !== 0) {
      console.error(stderr);
    }
  }
}

main().catch((error) => {
  console.error('MCP STREAMABLE HTTP TS E2E FAIL:', error);
  process.exitCode = 1;
});
