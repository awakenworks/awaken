// Runtime-neutral MCP server e2e over a real stdio subprocess.
//
// This is intentionally TypeScript driving the published compatibility binary,
// not an in-process Rust test. It crosses process + JSON-RPC framing boundaries
// and proves the extracted server core still powers awaken_protocol_mcp's facade.

import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createInterface } from 'node:readline';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Message = { jsonrpc?: string; id?: Json; method?: string; params?: any; result?: any; error?: any };

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

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
      // Cargo can interleave human diagnostics; only JSON artifact lines matter.
    }
  }
  throw new Error('could not resolve awaken-mcp-stdio-demo');
}

class Peer {
  readonly child: ChildProcessWithoutNullStreams;
  readonly notifications: Message[] = [];
  readonly unsolicited: Message[] = [];
  readonly stderr: string[] = [];
  private readonly pending = new Map<string, (message: Message) => void>();
  private nextId = 1;

  constructor(binary: string) {
    this.child = spawn(binary, [], { cwd: ROOT, stdio: ['pipe', 'pipe', 'pipe'] });
    createInterface({ input: this.child.stdout }).on('line', (line) => {
      const message = JSON.parse(line) as Message;
      if (message.method && message.id === undefined) {
        this.notifications.push(message);
        return;
      }
      const key = JSON.stringify(message.id);
      const resolve = this.pending.get(key);
      if (resolve) {
        this.pending.delete(key);
        resolve(message);
      } else {
        this.unsolicited.push(message);
      }
    });
    this.child.stderr.on('data', (chunk) => this.stderr.push(chunk.toString()));
  }

  request(method: string, params: Json, id: Json = this.nextId++): Promise<Message> {
    const key = JSON.stringify(id);
    const response = new Promise<Message>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(key);
        reject(new Error(`timed out waiting for ${method} id=${key}; stderr=${this.stderr.join('')}`));
      }, 10_000);
      this.pending.set(key, (message) => {
        clearTimeout(timer);
        resolve(message);
      });
    });
    this.send({ jsonrpc: '2.0', id, method, params });
    return response;
  }

  notify(method: string, params: Json): void {
    this.send({ jsonrpc: '2.0', method, params });
  }

  private send(message: Message): void {
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  async close(): Promise<void> {
    if (this.child.exitCode !== null) return;
    const exited = new Promise<void>((resolve) => this.child.once('exit', () => resolve()));
    this.child.stdin.end();
    await Promise.race([exited, new Promise<void>((resolve) => setTimeout(resolve, 2_000))]);
    if (this.child.exitCode === null) this.child.kill('SIGINT');
  }
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

async function main(): Promise<void> {
  const peer = new Peer(buildDemo());
  try {
    const initialized = await peer.request('initialize', {
      protocolVersion: '2025-11-25',
      capabilities: {},
      clientInfo: { name: 'awaken-ts-e2e', version: '1' },
    });
    assert.equal(initialized.result?.protocolVersion, '2025-11-25');
    assert.equal(initialized.result?.capabilities?.tools?.listChanged, true);

    const older = await peer.request('initialize', { protocolVersion: '2024-11-05' });
    assert.equal(older.result?.protocolVersion, '2024-11-05', 'supported older version is negotiated');
    const unsupported = await peer.request('initialize', { protocolVersion: '1900-01-01' });
    assert.equal(unsupported.error?.code, -32602, 'unsupported version is invalid params');

    peer.notify('notifications/initialized', {});
    const ping = await peer.request('ping', {});
    assert.deepEqual(ping.result, {});
    assert.equal(peer.unsolicited.length, 0, 'client notification produced no JSON-RPC response');

    const listed = await peer.request('tools/list', {});
    assert.deepEqual(
      listed.result.tools.map((tool: any) => tool.name).sort(),
      ['count', 'echo'],
    );
    const invalid = await peer.request('tools/call', { arguments: {} });
    assert.equal(invalid.error?.code, -32602, 'invalid params rejected before tool dispatch');
    const unknown = await peer.request('unknown/method', {});
    assert.equal(unknown.error?.code, -32601);

    const echoed = await peer.request('tools/call', {
      name: 'echo',
      arguments: { message: 'from TypeScript' },
    });
    assert.equal(echoed.result?.content?.[0]?.text, 'echo: from TypeScript');
    assert.equal(echoed.result?.isError, false);

    const before = peer.notifications.length;
    const counted = await peer.request('tools/call', {
      name: 'count',
      arguments: { steps: 4 },
      _meta: { progressToken: 'ts-progress' }, // awaken-allow: secret
    });
    const progress = peer.notifications.slice(before);
    assert.deepEqual(progress.map((event) => event.method), Array(4).fill('notifications/progress'));
    assert.deepEqual(progress.map((event) => event.params.progress), [1, 2, 3, 4]);
    assert.ok(progress.every((event) => event.params.progressToken === 'ts-progress'));
    assert.equal(counted.result?.content?.[0]?.text, 'counted to 4');
    const atFinal = peer.notifications.length;
    await sleep(80);
    assert.equal(peer.notifications.length, atFinal, 'no progress is emitted after the final response');

    const cancellationStart = peer.notifications.length;
    const cancelling = peer.request(
      'tools/call',
      {
        name: 'count',
        arguments: { steps: 100 },
        _meta: { progressToken: 'cancel-progress' }, // awaken-allow: secret
      },
      'cancel-me',
    );
    await sleep(40);
    peer.notify('notifications/cancelled', { requestId: 'cancel-me', reason: 'TS requested cancel' });
    const cancelled = await cancelling;
    assert.equal(cancelled.error?.code, -32603, 'cancelled request returns one internal-error response');
    assert.ok(
      peer.notifications
        .slice(cancellationStart)
        .some((event) => event.params?.progressToken === 'cancel-progress'),
      'the long call was active before cancellation',
    );
    await sleep(80);
    const afterCancellation = peer.notifications.length;
    await sleep(80);
    assert.equal(
      peer.notifications.length,
      afterCancellation,
      'cancellation stops further progress after the final error response',
    );

    console.log(
      'MCP SERVER CORE TS E2E PASS: negotiation, notification, list/call, validation, errors, cancellation, ordered progress and final-response ordering.',
    );
  } finally {
    await peer.close();
  }
}

main().catch((error) => {
  console.error('MCP SERVER CORE TS E2E FAIL:', error);
  process.exitCode = 1;
});
