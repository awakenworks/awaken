// Official @modelcontextprotocol/sdk behavior E2E over Awaken's Streamable HTTP adapter.
//
// Causal graph:
// SDK Client.connect -> initialize/session negotiation -> initialized notification
// SDK listTools/callTool -> typed JSON-RPC -> Awaken tool dispatch -> SDK result
// SDK close/terminate -> DELETE session -> server-side session retirement
//
// Decision table:
// | auth | operation       | arguments             | observable effect             |
// | good | connect         | supported version     | capabilities + session        |
// | good | listTools       | none                  | exact typed tool catalog      |
// | good | callTool echo   | valid object          | non-error content             |
// | good | callTool count  | valid object          | terminal content              |
// | good | terminate       | active session        | transport closes cleanly      |
// Raw-envelope tests remain in mcp_streamable_http_e2e.ts for malformed HTTP and
// JSON-RPC partitions that the official SDK intentionally prevents callers creating.

import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import { CallToolResultSchema } from '@modelcontextprotocol/sdk/types.js';
// @ts-expect-error The shared JavaScript harness intentionally has no declaration file.
import { stopServer, waitForPort } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38816);
const TOKEN = 'mcp-official-sdk-token'; // awaken-allow: secret

function buildDemo(): string {
  const output = execFileSync(
    'cargo',
    ['build', '--quiet', '--message-format=json', '-p', 'awaken-protocol-mcp', '--bin', 'awaken-mcp-stdio-demo'],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken-mcp-stdio-demo') return message.executable;
    } catch {
      // Cargo may interleave human diagnostics.
    }
  }
  throw new Error('could not resolve awaken-mcp-stdio-demo');
}

async function main(): Promise<void> {
  const child = spawn(buildDemo(), [], {
    cwd: ROOT,
    env: {
      ...process.env,
      AWAKEN_MCP_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_MCP_BEARER_TOKEN: TOKEN,
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => (stderr += chunk.toString()));
  const client = new Client({ name: 'awaken-official-sdk-e2e', version: '1.0.0' });
  const transport = new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${PORT}/mcp`), {
    requestInit: { headers: { authorization: `Bearer ${TOKEN}` } },
  });
  try {
    await waitForPort(PORT);
    await client.connect(transport);
    assert.equal(client.getServerCapabilities()?.tools?.listChanged, true);
    assert.ok(transport.sessionId, 'official transport retained the negotiated session id');

    const listed = await client.listTools();
    assert.deepEqual(listed.tools.map((tool) => tool.name).sort(), ['count', 'echo']);
    assert.equal(listed.tools[0]?.inputSchema?.type, 'object');

    const echoed = await client.callTool(
      { name: 'echo', arguments: { message: 'official SDK' } },
      CallToolResultSchema,
    );
    assert.equal(echoed.isError, false);
    assert.ok(Array.isArray(echoed.content));
    const echoContent = echoed.content as Array<{ type: string; text?: string }>;
    assert.equal(echoContent[0]?.type, 'text');
    assert.equal(echoContent[0]?.text, 'echo: official SDK');

    const counted = await client.callTool(
      { name: 'count', arguments: { steps: 2 } },
      CallToolResultSchema,
    );
    assert.equal(counted.isError, false);
    assert.ok(Array.isArray(counted.content));
    const countContent = counted.content as Array<{ type: string; text?: string }>;
    assert.equal(countContent[0]?.type, 'text');
    assert.equal(countContent[0]?.text, 'counted to 2');

    await transport.terminateSession();
    console.log('MCP OFFICIAL SDK TS E2E PASS: connect, typed list/call, session and termination.');
  } finally {
    await client.close().catch(() => {});
    await stopServer(child).catch(() => {});
    if (child.exitCode && child.exitCode !== 0) console.error(stderr);
  }
}

main().catch((error) => {
  console.error('MCP OFFICIAL SDK TS E2E FAIL:', error);
  process.exitCode = 1;
});
